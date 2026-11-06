//! What the media tools offer, listed from the snapshot (chat-media-tools
//! design §3.2, §3.4): which rows and aliases each tool's `model` enum
//! holds, and one description line per choice built from facts lmgw has.
//!
//! - `image__generate`: the enabled image rows whose modes hold `img_gen`,
//!   then the enabled aliases whose published endpoints (`GET /v1/models`)
//!   hold `/v1/images/generations`;
//! - `image__edit`: of those rows only the ones that edit (`edit` set), then
//!   the aliases publishing `/v1/images/edits` ([`edit_rows`](super::edit_rows));
//! - `audio__speak` / `audio__transcribe`: the enabled audio rows of task
//!   `tts` / `asr`, then the aliases publishing `/v1/audio/speech` /
//!   `/v1/audio/transcriptions`.
//!
//! **The list depends on configuration only**, so the tool block a chat
//! model is sent stays the same from turn to turn until the owner changes
//! something — a block that changed would change the prompt prefix, and a
//! local chat model's prompt cache would be thrown away with it:
//! - a cloud alias's endpoints are the ones its configuration publishes —
//!   its model's name and its `capabilities_override`
//!   (`capabilities::exposed::configured_endpoints`), never whether some
//!   other look happened to read its catalog; an alias onto one of lmgw's
//!   own upstream kinds publishes its backing row's, its catalog read only
//!   as cached (`catalog::peek_only`), so listing never waits on a provider
//!   or starts a read;
//! - an image row's defaults and size range are its container's probed
//!   capabilities, remembered after the container stops
//!   (`Registry::image_capabilities`): a row reads the same from its first
//!   start on, and before any start the line has no defaults yet.
//!
//! **Default models** (design §3.8): Settings → Chat's
//! `chat_media_defaults`, with a Chat folder's and a thread's laid over
//! them ([`Offer::layered`]), name the model a call that leaves out `model`
//! uses. A default counts only while it is one of the tool's choices for
//! the caller ([`Offer::default_for`]), so a row switched off, an alias that
//! lost the route or a model outside the caller's key scope is no default
//! and the tool requires `model` again. The tool's description names the
//! default and its schema stops requiring `model`; both change only when
//! the resolved default does.

use serde_json::{json, Value};

use lmgw_api_types::chat_media::{MediaDefaults, MediaField};

use crate::capabilities::{IMAGE_EDITS_ENDPOINT, IMAGE_GENERATIONS_ENDPOINT};
use crate::config::{AudioModel, ImageModel, Snapshot};
use crate::state::SharedState;

use super::defaults::MediaLayers;

use super::{AUDIO_LABEL, AUDIO_SPEAK, AUDIO_TRANSCRIBE, IMAGE_EDIT, IMAGE_GENERATE, IMAGE_LABEL};

pub(crate) const SPEECH_ENDPOINT: &str = "/v1/audio/speech";
pub(crate) const TRANSCRIPTIONS_ENDPOINT: &str = "/v1/audio/transcriptions";

/// One choice of a tool's `model` enum.
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    /// The public name the gate resolves (`image/z-image-turbo`, an alias).
    pub name: String,
    /// What the description says about it, after `- <name>`.
    pub line: String,
    /// An lmgw row (a local model) rather than an alias.
    pub row: bool,
}

/// What each tool offers right now.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Offer {
    pub generate: Vec<Choice>,
    pub edit: Vec<Choice>,
    pub speak: Vec<Choice>,
    pub transcribe: Vec<Choice>,
    /// `image__upscale` (design §3.2, §9): the upscalers at the top level
    /// of the image models dir's `upscalers/`, by name (the file stem) —
    /// its `upscaler` enum. The tool is offered only when there is one.
    pub upscalers: Vec<String>,
    /// The image rows an upscale may run on, by public name: the rows of
    /// `image__generate`.
    pub upscale_rows: Vec<String>,
    /// The largest picture an upscale on each of [`Self::upscale_rows`]
    /// makes (`limits.max_upscale_width` x `max_upscale_height`), by public
    /// name; `None` for a row whose build was not probed yet or names none.
    /// Which row runs an upscale is decided when it runs (design §9), so the
    /// tool's description says every row's ([`Self::upscale_limit_said`]).
    pub upscale_limits: Vec<(String, Option<(i64, i64)>)>,
    /// The default models asked for (module doc): Settings → Chat's, with
    /// the levels of [`Self::layered`] laid over. A name here is a default
    /// only while it is one of its tool's choices ([`Self::default_for`]).
    pub wanted: MediaDefaults,
    /// Each speech row's voices, by its public name: whether the default
    /// voice is one of the default speech row's ([`Self::voice_for`]).
    pub speak_voices: Vec<(String, Vec<String>)>,
    /// [`Self::wanted`] is what a run's tool block named at its start
    /// ([`Self::frozen_to`]) rather than what the levels say now.
    pub frozen: bool,
}

/// The default field of tool `name`.
pub(crate) fn field_of(name: &str) -> Option<MediaField> {
    Some(match name {
        IMAGE_GENERATE => MediaField::Generate,
        IMAGE_EDIT => MediaField::Edit,
        AUDIO_SPEAK => MediaField::Speak,
        AUDIO_TRANSCRIBE => MediaField::Transcribe,
        _ => return None,
    })
}

impl Offer {
    /// The choices of tool `name`.
    pub fn choices(&self, name: &str) -> &[Choice] {
        match name {
            IMAGE_GENERATE => &self.generate,
            IMAGE_EDIT => &self.edit,
            AUDIO_SPEAK => &self.speak,
            AUDIO_TRANSCRIBE => &self.transcribe,
            _ => &[],
        }
    }

    /// The `tools/list` entries of `label` (`image` or `audio`): a tool with
    /// no choice is not listed.
    pub fn tools(&self, label: &str) -> Vec<Value> {
        let names: &[&str] = match label {
            IMAGE_LABEL => &[IMAGE_GENERATE, IMAGE_EDIT],
            AUDIO_LABEL => &[AUDIO_SPEAK, AUDIO_TRANSCRIBE],
            _ => &[],
        };
        let mut out: Vec<Value> = names
            .iter()
            .filter(|n| !self.choices(n).is_empty())
            .map(|n| self.tool(n))
            .collect();
        if label == IMAGE_LABEL && self.offers_upscale() {
            out.push(super::upscale::tool(self));
        }
        out
    }

    /// The model tool `name` uses when a call leaves out `model`: the
    /// default asked for, while it is one of the tool's choices (module
    /// doc). An empty name is a level's "no default".
    pub fn default_for(&self, name: &str) -> Option<&Choice> {
        let want = self.wanted.get(field_of(name)?).filter(|n| !n.is_empty())?;
        self.choices(name).iter().find(|c| c.name == want)
    }

    /// The voice `audio__speak` on `model` uses when the call names none:
    /// the default voice, when `model` is the default speech model it
    /// belongs to — a row that has that voice, or an alias (whose voices
    /// are the provider's own, so lmgw cannot tell).
    pub fn voice_for(&self, model: &str) -> Option<&str> {
        let voice = self.wanted.voice.as_deref().filter(|v| !v.is_empty())?;
        let d = self.default_for(AUDIO_SPEAK).filter(|d| d.name == model)?;
        if !d.row {
            return Some(voice);
        }
        self.voices_of(model)
            .is_some_and(|vs| vs.iter().any(|v| v == voice))
            .then_some(voice)
    }

    /// The voices of speech row `name`, when it is one.
    pub fn voices_of(&self, name: &str) -> Option<&[String]> {
        self.speak_voices
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_slice())
    }

    /// What a call that leaves the argument out uses now: each tool's
    /// default model ([`Self::default_for`]) and the default voice with
    /// the default speech model ([`Self::voice_for`]); a field absent has
    /// no default.
    pub fn resolved_defaults(&self) -> MediaDefaults {
        let mut out = MediaDefaults::default();
        for (f, tool) in [
            (MediaField::Generate, IMAGE_GENERATE),
            (MediaField::Edit, IMAGE_EDIT),
            (MediaField::Speak, AUDIO_SPEAK),
            (MediaField::Transcribe, AUDIO_TRANSCRIBE),
        ] {
            *out.slot(f) = self.default_for(tool).map(|c| c.name.clone());
        }
        out.voice = out
            .speak
            .as_deref()
            .and_then(|m| self.voice_for(m))
            .map(str::to_string);
        out
    }

    /// The defaults a run's tool block named at its start
    /// ([`Self::resolved_defaults`] then), in place of what the levels say
    /// now: a call that leaves out `model` runs on the model its
    /// description promised, or is told that model is gone — never on
    /// another one a settings change made the default meanwhile.
    pub fn frozen_to(mut self, resolved: &MediaDefaults) -> Self {
        self.wanted = resolved.clone();
        self.frozen = true;
        self
    }

    /// The default model the run's tool block named for tool `name`, when
    /// it is no longer one of the tool's choices ([`Self::frozen_to`]).
    pub fn default_gone(&self, name: &str) -> Option<&str> {
        if !self.frozen {
            return None;
        }
        let want = self.wanted.get(field_of(name)?).filter(|n| !n.is_empty())?;
        (!self.choices(name).iter().any(|c| c.name == want)).then_some(want)
    }

    /// `layers` laid over the defaults, the narrowest last: a Chat
    /// folder's, then a thread's (module doc).
    pub fn layered(mut self, layers: &MediaLayers) -> Self {
        for l in layers.levels() {
            self.wanted = std::mem::take(&mut self.wanted).laid_over(l);
        }
        self
    }

    /// Whether `image__upscale` is offered: an upscaler file and a row to
    /// run it on.
    pub fn offers_upscale(&self) -> bool {
        !self.upscalers.is_empty() && !self.upscale_rows.is_empty()
    }

    /// The choices the client key `key` may use: its alias scope
    /// (`policy::check_scope`) keeps the rest out of every enum, so a
    /// scoped caller is never offered a model its calls would be refused.
    /// `None` keeps everything (the gateway's own runs, the owner).
    pub fn within(mut self, snap: &Snapshot, key: Option<&str>) -> Self {
        if key.is_none() {
            return self;
        }
        let keep = |c: &Choice| crate::policy::check_scope(snap, key, &c.name).is_ok();
        for list in [
            &mut self.generate,
            &mut self.edit,
            &mut self.speak,
            &mut self.transcribe,
        ] {
            list.retain(keep);
        }
        // A speech row's voices go with it: a row the key may not use
        // names none of its voices to that key either.
        self.speak_voices
            .retain(|(n, _)| crate::policy::check_scope(snap, key, n).is_ok());
        self.upscale_rows
            .retain(|r| crate::policy::check_scope(snap, key, r).is_ok());
        let rows = &self.upscale_rows;
        self.upscale_limits.retain(|(r, _)| rows.contains(r));
        self
    }

    /// The upscale limit in a sentence for the tool's description: the one
    /// limit when every row names the same, each row's when they differ
    /// or some row's is not known yet; empty when none is known.
    pub fn upscale_limit_said(&self) -> String {
        let known: Vec<(i64, i64)> = self.upscale_limits.iter().filter_map(|(_, l)| *l).collect();
        let Some(first) = known.first() else {
            return String::new();
        };
        let all_known = known.len() == self.upscale_limits.len();
        if all_known && known.iter().all(|l| l == first) {
            let (w, h) = first;
            return format!(
                " The result may not be larger than {w}x{h}, the image server's limit."
            );
        }
        let each: Vec<String> = self
            .upscale_limits
            .iter()
            .map(|(r, l)| match l {
                Some((w, h)) => format!("{w}x{h} on {r}"),
                None => format!("not known yet on {r}"),
            })
            .collect();
        format!(
            " The result may not be larger than the limit of the image server that runs it, \
             which the gateway picks: {}.",
            each.join(", ")
        )
    }

    /// Every tool of both labels, as `/mcp` lists them.
    pub fn all(&self) -> Vec<Value> {
        let mut out = self.tools(IMAGE_LABEL);
        out.extend(self.tools(AUDIO_LABEL));
        out
    }

    /// Why `label` offers nothing, in a sentence per tool (design §3.2).
    pub fn nothing_offered(label: &str) -> String {
        match label {
            IMAGE_LABEL => "no enabled image model serves generations — add one under Models → \
                            Image, or give an image alias the /v1/images/generations route"
                .to_string(),
            _ => "no enabled audio model speaks or transcribes — add a TTS or ASR model under \
                  Models → Audio, or name a cloud speech alias"
                .to_string(),
        }
    }
}

/// What the tools offer now (module doc).
pub async fn offer(state: &SharedState) -> Offer {
    let snap = state.snapshot();
    let mut out = Offer::default();
    for row in snap
        .image_models
        .iter()
        .filter(|m| m.enabled && m.modes().iter().any(|m| m == "img_gen"))
    {
        let name = snap.image_public_name(&row.model_id);
        let probed = probed_image(state, &row.model_id);
        out.upscale_rows.push(name.clone());
        out.upscale_limits.push((
            name.clone(),
            probed
                .as_ref()
                .and_then(|c| c.limits.max_upscale_width.zip(c.limits.max_upscale_height)),
        ));
        out.generate.push(Choice {
            name: name.clone(),
            line: image_line(row, probed.as_ref(), false),
            row: true,
        });
        if super::edit_rows::edits(row) {
            out.edit.push(Choice {
                name,
                line: image_line(row, probed.as_ref(), true),
                row: true,
            });
        }
    }
    out.upscalers = crate::image_upscalers::names(&snap.settings.image.models_dir);
    out.wanted = snap.settings.chat_media_defaults.clone().normalised(true);
    for row in snap.audio_models.iter().filter(|m| m.enabled) {
        let name = snap.audio_public_name(&row.model_id);
        match row.task.as_str() {
            "tts" => {
                let (line, voices) = speak_line(state, row).await;
                out.speak_voices.push((name.clone(), voices));
                out.speak.push(Choice {
                    line,
                    name,
                    row: true,
                });
            }
            "asr" => out.transcribe.push(Choice {
                line: transcribe_line(state, row).await,
                name,
                row: true,
            }),
            _ => {}
        }
    }
    let aliases = crate::catalog::peek_only(cloud_aliases(state, &snap)).await;
    for (alias, upstream, endpoints) in aliases {
        let has = |e: &str| endpoints.iter().any(|x| x == e);
        let via = format!("cloud model via {upstream}");
        if has(IMAGE_GENERATIONS_ENDPOINT) {
            out.generate.push(Choice {
                name: alias.clone(),
                line: format!(
                    " ({via}): write the prompt as plain sentences. It takes no seed, steps, \
                     cfg or negative_prompt."
                ),
                row: false,
            });
        }
        if has(IMAGE_EDITS_ENDPOINT) {
            out.edit.push(Choice {
                name: alias.clone(),
                line: format!(
                    " ({via}): say in prompt what to change in the picture. It takes no seed, \
                     steps, cfg, negative_prompt or strength."
                ),
                row: false,
            });
        }
        if has(SPEECH_ENDPOINT) {
            out.speak.push(Choice {
                name: alias.clone(),
                line: format!(" ({via}): voice names are the provider's own."),
                row: false,
            });
        }
        if has(TRANSCRIPTIONS_ENDPOINT) {
            out.transcribe.push(Choice {
                name: alias.clone(),
                line: format!(" ({via})."),
                row: false,
            });
        }
    }
    out
}

/// The enabled aliases onto a non-local upstream, with that upstream's
/// name and the endpoints the alias publishes.
async fn cloud_aliases(state: &SharedState, snap: &Snapshot) -> Vec<(String, String, Vec<String>)> {
    let mut out = Vec::new();
    for a in snap.enabled_aliases() {
        // A local class's synthetic upstream has a negative id; its rows
        // are listed above under their own names.
        let Some(up) = snap.upstreams.get(&a.upstream_id).filter(|u| u.enabled) else {
            continue;
        };
        let endpoints = match up.kind {
            crate::config::UpstreamKind::Generic => {
                crate::capabilities::exposed::configured_endpoints(
                    &a.upstream_model_id,
                    a.capabilities_override.as_ref(),
                    up,
                )
            }
            _ => {
                let Some(entry) =
                    crate::capabilities::exposed::exposed_entry(state, &a.alias).await
                else {
                    continue;
                };
                entry.capabilities.map(|c| c.endpoints).unwrap_or_default()
            }
        };
        out.push((a.alias.clone(), up.name.clone(), endpoints));
    }
    out
}

/// The row's probed capabilities: its running container's, else the last
/// one's (module doc).
fn probed_image(
    state: &SharedState,
    model_id: &str,
) -> Option<crate::runtime::image::ImageCapabilities> {
    state.runtime().image_capabilities(model_id)
}

/// The shipped family an image row's main file matches, by its name.
pub(crate) fn image_family(row: &ImageModel) -> Option<&'static str> {
    ["diffusion_model", "model"]
        .iter()
        .filter_map(|k| row.files.get(*k).and_then(Value::as_str))
        .find_map(crate::image_recipes::match_primary)
        .map(|r| r.display_name)
}

/// One image row's line (design §3.4): its family, its prompt style, its
/// defaults and size range, and for an edit how the picture is used.
fn image_line(
    row: &ImageModel,
    probed: Option<&crate::runtime::image::ImageCapabilities>,
    edit: bool,
) -> String {
    let family = image_family(row)
        .map(|f| format!(" ({f})"))
        .unwrap_or_default();
    let style = match crate::image_recipes::prompt_style_of(row) {
        Some(s) => s.instruction().to_string(),
        None => "prompt style not set: describe the picture in plain sentences".to_string(),
    };
    let mut out = format!("{family}: {style}.");
    if edit {
        out.push_str(" Edits by instruction: say in prompt what to change.");
    }
    let defaults = probed.and_then(|c| c.defaults_by_mode.get("img_gen"));
    let d = |path: &[&str]| -> Option<Value> {
        let mut v = defaults?.get(path[0])?;
        for k in &path[1..] {
            v = v.get(k)?;
        }
        Some(v.clone())
    };
    let arg = |k: &str| row.args.get(k).cloned();
    let width = arg("width").or_else(|| d(&["width"]));
    let height = arg("height").or_else(|| d(&["height"]));
    let steps = arg("steps").or_else(|| d(&["sample_params", "sample_steps"]));
    let cfg = arg("cfg_scale").or_else(|| d(&["sample_params", "guidance", "txt_cfg"]));
    let mut facts = Vec::new();
    if let (Some(w), Some(h)) = (&width, &height) {
        facts.push(format!("{}x{}", plain(w), plain(h)));
    }
    if let Some(s) = &steps {
        facts.push(format!("{} steps", plain(s)));
    }
    if let Some(c) = &cfg {
        let one = c.as_f64() == Some(1.0);
        facts.push(if one {
            "cfg 1 (negative_prompt has no effect at cfg 1)".to_string()
        } else {
            format!("cfg {}", plain(c))
        });
    }
    if !facts.is_empty() {
        out.push_str(&format!(" Defaults: {}.", facts.join(", ")));
    }
    if let Some(l) = probed.map(|c| &c.limits) {
        if let (Some(lo), Some(hi)) = (l.min_width.min(l.min_height), l.max_width.max(l.max_height))
        {
            out.push_str(&format!(" Sizes {lo}–{hi} px a side."));
        }
    }
    out
}

/// A JSON scalar as a person writes it: `1` for `1.0`, a string unquoted.
fn plain(v: &Value) -> String {
    match v {
        Value::Number(n) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 && n.is_f64() => format!("{}", f as i64),
            _ => n.to_string(),
        },
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A TTS row's line: its voices and default, whether it takes instructions,
/// and its languages; and the voices.
async fn speak_line(state: &SharedState, row: &AudioModel) -> (String, Vec<String>) {
    use crate::audio::profile::InstructionsMode;
    let speech = crate::audio::voices::row_speech(state, row).await;
    let mut parts = Vec::new();
    let names = speech.voices.names();
    if !names.is_empty() {
        parts.push(format!("voices {}", names.join(", ")));
    }
    if let Some(d) = &speech.voices.default {
        parts.push(format!("default voice {d}"));
    }
    parts.push(match speech.profile.instructions {
        InstructionsMode::None => "takes no instructions".to_string(),
        InstructionsMode::Style => "instructions: a speaking style".to_string(),
        InstructionsMode::VoiceDesign => {
            "instructions are required: describe the voice to speak with".to_string()
        }
        InstructionsMode::Passthrough => "takes instructions".to_string(),
    });
    match languages(&speech.profile) {
        Some(l) => parts.push(format!("language: {l}")),
        None => parts.push("reads the text in the language it is written in".to_string()),
    }
    (format!(": {}.", parts.join("; ")), names)
}

/// An ASR row's line: the languages its spec names, where it names any.
async fn transcribe_line(state: &SharedState, row: &AudioModel) -> String {
    let profile = crate::audio::voices::row_profile(state, row).await;
    match languages(&profile) {
        Some(l) => format!(": hears {l}."),
        None => ": the language is detected when none is given.".to_string(),
    }
}

/// The languages a row's spec or package names, as a list.
fn languages(profile: &crate::audio::profile::SpeechProfile) -> Option<String> {
    if let Some(l) = &profile.package_language {
        return Some(l.clone());
    }
    (!profile.spec_languages.is_empty()).then(|| profile.spec_languages.join(", "))
}

impl Offer {
    /// Tool `name`'s `tools/list` entry, with its default model and voice
    /// (module doc).
    fn tool(&self, name: &str) -> Value {
        let default = self.default_for(name);
        let voice = default.and_then(|d| self.voice_for(&d.name).map(|v| (v, d.name.as_str())));
        tool(
            name,
            self.choices(name),
            !self.edit.is_empty(),
            default.map(|d| d.name.as_str()),
            voice,
        )
    }
}

/// One tool's `tools/list` entry, its `model` enum the choices'.
/// `edit_offered`: whether `image__edit` is listed beside it, which
/// `image__generate`'s description says only then. With a `default`,
/// `model` may be left out and the description says which model that
/// uses; with a default `voice` (the voice, its model), the voice's
/// description says which voice an omitted one is.
fn tool(
    name: &str,
    choices: &[Choice],
    edit_offered: bool,
    default: Option<&str>,
    voice: Option<(&str, &str)>,
) -> Value {
    let models: Vec<&str> = choices.iter().map(|c| c.name.as_str()).collect();
    let lines: Vec<String> = choices
        .iter()
        .map(|c| format!("- {}{}", c.name, c.line))
        .collect();
    let model = json!({
        "type": "string",
        "enum": models,
        "description": match default {
            Some(d) => format!("Which model to use (see the list above). Omit it to use {d}."),
            None => "Which model to use (see the list above).".to_string(),
        },
    });
    let voice_said = match voice {
        Some((v, m)) => format!(
            "A voice the model has (see the list above). Omit it for {v} with {m}, or the \
             model's default with another model."
        ),
        None => "A voice the model has (see the list above). Omit it for the model's default."
            .to_string(),
    };
    let (lead, props, required) = match name {
        IMAGE_GENERATE => (
            if edit_offered {
                "Draw a new picture with one of this gateway's image models. In a Chat \
                 conversation the user sees it at once and the result names it as att:N, which \
                 image__edit takes; elsewhere the picture comes back in the result. A picture \
                 loads its image model onto the GPU and can unload the chat model meanwhile, so \
                 make the pictures you were asked for, not extra rounds."
            } else {
                "Draw a new picture with one of this gateway's image models. In a Chat \
                 conversation the user sees it at once and the result names it as att:N; \
                 elsewhere the picture comes back in the result. None of these models can edit \
                 a picture: to change one, draw it again with a new prompt. A picture loads its \
                 image model onto the GPU and can unload the chat model meanwhile, so make the \
                 pictures you were asked for, not extra rounds."
            },
            image_props(model, false),
            json!(["model", "prompt"]),
        ),
        IMAGE_EDIT => (
            "Change a picture as the prompt says with one of this gateway's image edit models \
             (only the models listed here can edit). Like image__generate it loads the image \
             model onto the GPU.",
            image_props(model, true),
            json!(["model", "prompt", "image"]),
        ),
        AUDIO_SPEAK => (
            "Speak a text aloud with one of this gateway's text-to-speech models. In a Chat \
             conversation the user gets a player for it and the result names it as att:N; \
             elsewhere the audio comes back in the result. You get no audio back.",
            json!({
                "model": model,
                "text": {"type": "string", "description": "What to say."},
                "voice": {"type": "string", "description": voice_said},
                "instructions": {"type": "string", "description": "How to speak, for a model that takes instructions (see above)."},
                "language": {"type": "string", "description": "The text's language as a two-letter code (en, de, ...), for a model that reads one; others ignore it."},
            }),
            json!(["model", "text"]),
        ),
        _ => (
            "Transcribe a recording with one of this gateway's speech-to-text models and get \
             its text back.",
            json!({
                "model": model,
                "audio": {"type": "string", "description": "The recording: att:N from this conversation, or a data: URL."},
                "language": {"type": "string", "description": "The spoken language as a two-letter code (en, de, ...). Omit it to let the model detect it."},
            }),
            json!(["model", "audio"]),
        ),
    };
    // `model` is required only while there is no default to use.
    let required: Vec<Value> = required
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| default.is_none() || r.as_str() != Some("model"))
        .cloned()
        .collect();
    let omit = default
        .map(|d| format!(" Omit `model` to use {d}."))
        .unwrap_or_default();
    json!({
        "name": name,
        "description": format!("{lead}{omit}\n{}", lines.join("\n")),
        "inputSchema": {
            "type": "object",
            "properties": props,
            "required": required,
            "additionalProperties": false,
        },
    })
}

/// `image__generate`'s parameters (design §3.3), with `image__edit`'s
/// picture.
fn image_props(model: Value, edit: bool) -> Value {
    let mut p = json!({
        "model": model,
        "prompt": {"type": "string", "description": "What to draw, in the style the chosen model reads (see above)."},
        "negative_prompt": {"type": "string", "description": "What to keep out of the picture."},
        "size": {"type": "string", "description": "WIDTHxHEIGHT in pixels, e.g. 1024x768. Omit for the model's default."},
        "seed": {"type": "integer", "description": "The same seed with the same settings gives the same picture. Omit for a new one."},
        "steps": {"type": "integer", "description": "Sampling steps. Omit for the model's default."},
        "cfg": {"type": "number", "description": "How strictly the picture follows the prompt. Omit for the model's default."},
    });
    if edit {
        p["prompt"] = json!({"type": "string", "description": "What to change in the picture, as an instruction."});
        p["image"] = json!({"type": "string", "description": "The picture to change: att:N from this conversation, or a data: URL."});
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_whole_number_reads_without_its_point() {
        assert_eq!(plain(&json!(1.0)), "1");
        assert_eq!(plain(&json!(3.5)), "3.5");
        assert_eq!(plain(&json!(1024)), "1024");
        assert_eq!(plain(&json!("euler")), "euler");
    }

    #[test]
    fn every_tool_is_closed_and_requires_its_model() {
        let c = vec![Choice {
            name: "image/x".into(),
            line: ": write the prompt as plain sentences.".into(),
            row: true,
        }];
        for name in [IMAGE_GENERATE, IMAGE_EDIT, AUDIO_SPEAK, AUDIO_TRANSCRIBE] {
            let t = tool(name, &c, true, None, None);
            assert!(super::super::owns(name));
            assert_eq!(t["inputSchema"]["additionalProperties"], json!(false));
            let req = t["inputSchema"]["required"].as_array().unwrap();
            assert!(req.contains(&json!("model")), "{name}");
            assert_eq!(
                t["inputSchema"]["properties"]["model"]["enum"],
                json!(["image/x"])
            );
            assert!(t["description"]
                .as_str()
                .unwrap()
                .contains("- image/x: write"));
        }
    }
}
