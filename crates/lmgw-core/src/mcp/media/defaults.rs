//! The media tools' default models (chat-media-tools design §3.8): the
//! levels laid over Settings → Chat's, what the pickers offer, why a stored
//! default no longer resolves, and the check a write of one goes through.
//!
//! **Resolution** is [`super::Offer`]'s: Settings → Chat's
//! `chat_media_defaults`, then a Chat folder's, then a thread's
//! ([`MediaLayers`]), the narrowest field that says something winning; the
//! name it lands on is a default only while it is one of its tool's choices
//! for the caller. Nothing falls back to another row: a default that does
//! not resolve is none, the tool requires `model` again, and every picker
//! shows the stored name with [`unavailable`]'s reason until it is picked
//! again or its model comes back.

use lmgw_api_types::chat_media::{
    MediaChoice, MediaChoices, MediaDefaults, MediaDefaultsView, MediaField, MediaUnavailable,
};

use crate::config::Snapshot;
use crate::state::SharedState;

use super::list::{image_family, Choice, Offer, SPEECH_ENDPOINT, TRANSCRIPTIONS_ENDPOINT};
use super::AUDIO_SPEAK;

/// The levels laid over Settings → Chat's defaults: a Chat thread's folder's,
/// then the thread's own. Empty for every caller without a thread (`/mcp`,
/// `/v1/responses`, agent runs), which use Settings' alone.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MediaLayers {
    pub folder: Option<MediaDefaults>,
    pub thread: Option<MediaDefaults>,
}

impl MediaLayers {
    /// A thread's levels: its folder's (when it is in one that sets any)
    /// and its own.
    pub fn of_thread(folder: Option<&MediaDefaults>, thread: &MediaDefaults) -> Self {
        Self {
            folder: folder.cloned().map(|f| f.normalised(false)),
            thread: Some(thread.clone().normalised(false)),
        }
    }

    /// The levels, the widest first.
    pub fn levels(&self) -> impl Iterator<Item = &MediaDefaults> {
        self.folder.iter().chain(self.thread.iter())
    }
}

/// The picker's choices: each tool's, in its enum's order, with a label a
/// person reads, and each speech row's voices.
pub fn choices(snap: &Snapshot, offer: &Offer) -> MediaChoices {
    let list = |cs: &[Choice]| -> Vec<MediaChoice> {
        cs.iter()
            .map(|c| MediaChoice {
                name: c.name.clone(),
                label: label(snap, c),
                local: c.row,
            })
            .collect()
    };
    MediaChoices {
        generate: list(&offer.generate),
        edit: list(&offer.edit),
        speak: list(&offer.speak),
        transcribe: list(&offer.transcribe),
        voices: offer.speak_voices.iter().cloned().collect(),
    }
}

/// `c` as a person reads it: its name, then its family on this machine, or
/// the upstream a cloud alias goes through.
fn label(snap: &Snapshot, c: &Choice) -> String {
    if c.row {
        let family = snap
            .image_models
            .iter()
            .find(|r| snap.image_public_name(&r.model_id) == c.name)
            .and_then(image_family);
        return match family {
            Some(f) => format!("{} · {f} on this machine", c.name),
            None => format!("{} · on this machine", c.name),
        };
    }
    let via = snap
        .aliases
        .get(&c.name)
        .and_then(|a| snap.upstreams.get(&a.upstream_id))
        .map(|u| u.name.as_str());
    match via {
        Some(u) => format!("{} · cloud via {u}", c.name),
        None => format!("{} · cloud", c.name),
    }
}

/// Why `name`, stored as `field`'s default, is none for the caller whose
/// choices are `mine`; `None` when it resolves. `all` is the offer before
/// the caller's key scope narrowed it.
pub fn unavailable(
    snap: &Snapshot,
    (all, mine): (&Offer, &Offer),
    field: MediaField,
    name: &str,
) -> Option<String> {
    if name.is_empty() || field == MediaField::Voice {
        return None;
    }
    let tool = field.tool();
    let has = |o: &Offer| o.choices(tool).iter().any(|c| c.name == name);
    if has(mine) {
        return None;
    }
    if has(all) {
        return Some("outside the model scope of this key".into());
    }
    let not_for_tool = || format!("{name} is not a model {tool} can use");
    let image_row = snap
        .image_models
        .iter()
        .find(|r| snap.image_public_name(&r.model_id) == name);
    let audio_row = snap
        .audio_models
        .iter()
        .find(|r| snap.audio_public_name(&r.model_id) == name);
    match field {
        MediaField::Generate | MediaField::Edit => {
            if let Some(row) = image_row {
                let draws = row.modes().iter().any(|m| m == "img_gen");
                return Some(if !row.enabled {
                    "the image model is switched off".into()
                } else if !draws {
                    "the image model no longer draws pictures (its modes leave out img_gen)".into()
                } else if field == MediaField::Edit {
                    // Drawing, but not among the edit models (`edit_rows`).
                    "the image model cannot edit pictures (Edit is not set on its row)".into()
                } else {
                    not_for_tool()
                });
            }
        }
        _ => {
            if let Some(row) = audio_row {
                let (wanted, needs) = if field == MediaField::Speak {
                    ("tts", "a TTS model")
                } else {
                    ("asr", "an ASR model")
                };
                return Some(if !row.enabled {
                    "the audio model is switched off".into()
                } else if row.task != wanted {
                    format!(
                        "the audio model's task is {} now, and {tool} needs {needs}",
                        row.task.to_uppercase()
                    )
                } else {
                    not_for_tool()
                });
            }
        }
    }
    if let Some(a) = snap.aliases.get(name) {
        if !a.enabled {
            return Some("the alias is switched off".into());
        }
        if !snap
            .upstreams
            .get(&a.upstream_id)
            .is_some_and(|u| u.enabled)
        {
            return Some("the alias's upstream is switched off".into());
        }
        let route = match field {
            MediaField::Generate => crate::capabilities::IMAGE_GENERATIONS_ENDPOINT,
            MediaField::Edit => crate::capabilities::IMAGE_EDITS_ENDPOINT,
            MediaField::Speak => SPEECH_ENDPOINT,
            _ => TRANSCRIPTIONS_ENDPOINT,
        };
        return Some(format!("the alias does not serve {route} now"));
    }
    // A row of the other class, or one of lmgw's chat models.
    if image_row.is_some()
        || audio_row.is_some()
        || snap.local_models.iter().any(|m| m.model_id == name)
    {
        return Some(not_for_tool());
    }
    Some("no model of this name exists any more".into())
}

/// Why the default voice `voice` speaks with nothing under `offer` (laid
/// and narrowed for the caller); `None` when the default speech model
/// takes it.
fn voice_unavailable(offer: &Offer, voice: &str) -> Option<String> {
    if voice.is_empty() {
        return None;
    }
    let Some(d) = offer.default_for(AUDIO_SPEAK) else {
        return Some("there is no default speech model to speak with it".into());
    };
    offer
        .voice_for(&d.name)
        .is_none()
        .then(|| format!("not a voice of {}", d.name))
}

/// What `GET /chat/api/media/defaults` answers for a caller with key `key`
/// (`None`: the owner) at the levels asked: Settings always, a folder's and
/// a thread's when given (a thread's folder comes with it).
pub async fn view(
    state: &SharedState,
    key: Option<&str>,
    folder: Option<MediaDefaults>,
    thread: Option<MediaDefaults>,
) -> MediaDefaultsView {
    let snap = state.snapshot();
    let all = super::offer(state).await;
    let mine = all.clone().within(&snap, key);
    let settings = snap.settings.chat_media_defaults.clone().normalised(true);
    let layers = MediaLayers {
        folder: folder.clone().map(|f| f.normalised(false)),
        thread: thread.clone().map(|t| t.normalised(false)),
    };
    let laid = mine.clone().layered(&layers);
    // What the narrowest level asked inherits.
    let inherited = match (&layers.folder, &layers.thread) {
        (_, Some(_)) => settings
            .clone()
            .laid_over(layers.folder.as_ref().unwrap_or(&MediaDefaults::default())),
        (Some(_), None) => settings.clone(),
        (None, None) => MediaDefaults::default(),
    };
    let resolved = laid.resolved_defaults();
    // Every stored name of every level asked, once each.
    let mut stale: Vec<MediaUnavailable> = Vec::new();
    for level in std::iter::once(&settings).chain(layers.levels()) {
        for f in MediaField::ALL {
            let Some(name) = level.get(f).filter(|n| !n.is_empty()) else {
                continue;
            };
            if stale
                .iter()
                .any(|u| u.field == f.as_str() && u.name == name)
            {
                continue;
            }
            let reason = match f {
                MediaField::Voice => voice_unavailable(&laid, name),
                _ => unavailable(&snap, (&all, &mine), f, name),
            };
            if let Some(reason) = reason {
                stale.push(MediaUnavailable {
                    field: f.as_str().to_string(),
                    name: name.to_string(),
                    reason,
                });
            }
        }
    }
    MediaDefaultsView {
        choices: choices(&snap, &mine),
        settings: snap.settings.chat_media_defaults.clone(),
        folder,
        thread,
        inherited,
        resolved,
        unavailable: stale,
    }
}

/// Settings → Chat's defaults with `patch` applied: each field the patch
/// names replaces the stored one (`""` clears it), checked as [`check`]
/// checks a level; the others stay.
pub async fn settings_patch(
    state: &SharedState,
    old: &MediaDefaults,
    patch: &MediaDefaults,
) -> Result<MediaDefaults, String> {
    let merged = old.clone().laid_over(patch);
    check(state, None, (merged, old), &MediaDefaults::default(), true)
        .await
        .map_err(|e| format!("chat_media_defaults: {e}"))
}

/// What a folder inherits: Settings → Chat's.
pub fn above_folder(state: &SharedState) -> MediaDefaults {
    state
        .snapshot()
        .settings
        .chat_media_defaults
        .clone()
        .normalised(true)
}

/// What a thread in folder `folder` (when in one) inherits: its folder's
/// laid over Settings → Chat's. A folder that cannot be read is none.
pub async fn above_thread(state: &SharedState, folder: Option<i64>) -> MediaDefaults {
    let mut out = above_folder(state);
    if let Some(id) = folder {
        if let Ok(Some(f)) = crate::store::get_chat_folder(&state.db, id).await {
            if let Some(m) = f.defaults.media_defaults {
                out = out.laid_over(&m.normalised(false));
            }
        }
    }
    out
}

/// The check a write of one level's defaults goes through (Settings, a
/// folder's, a thread's): each field `new` sets to a name `old` does not
/// hold must be one of its tool's choices for the caller (`key`), and a
/// voice must be one of the level's speech model's — its own, else the one
/// it inherits from `above` (the levels above laid, nothing for Settings) —
/// or, with no speech model at the level, one some speech model can speak
/// with. A name `old` already holds passes: a model switched off since
/// does not block the rest of a save, and stays shown as unavailable. The
/// level normalised.
pub async fn check(
    state: &SharedState,
    key: Option<&str>,
    (new, old): (MediaDefaults, &MediaDefaults),
    above: &MediaDefaults,
    settings: bool,
) -> Result<MediaDefaults, String> {
    let new = new.normalised(settings);
    let changed: Vec<(MediaField, &str)> = MediaField::ALL
        .iter()
        .filter_map(|f| new.get(*f).map(|n| (*f, n)))
        .filter(|(f, n)| !n.is_empty() && old.get(*f).map(str::trim) != Some(*n))
        .collect();
    if changed.is_empty() {
        return Ok(new);
    }
    let snap = state.snapshot();
    let all = super::offer(state).await;
    let mine = all.clone().within(&snap, key);
    for (f, name) in changed {
        if f == MediaField::Voice {
            // The level's speech model, its own or inherited: a row's voices
            // are known, a cloud alias's are the provider's own.
            let speech = new
                .speak
                .as_deref()
                .or(above.speak.as_deref())
                .filter(|m| !m.is_empty());
            if let Some(m) = speech {
                if let Some(vs) = mine.voices_of(m) {
                    if vs.iter().any(|v| v == name) {
                        continue;
                    }
                    return Err(format!(
                        "media default voice '{name}' is not a voice of {m}, the speech model it \
                         goes with; its voices: {}",
                        if vs.is_empty() {
                            "none listed".to_string()
                        } else {
                            vs.join(", ")
                        }
                    ));
                }
                if mine.speak.iter().any(|c| c.name == m && !c.row) {
                    continue;
                }
            }
            // A cloud speech alias's voices are the provider's own.
            let known = mine.speak.iter().any(|c| !c.row)
                || mine
                    .speak_voices
                    .iter()
                    .any(|(_, vs)| vs.iter().any(|v| v == name));
            if !known {
                return Err(format!(
                    "media default voice '{name}' is not a voice of any speech model{}",
                    if mine.speak.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " ({})",
                            mine.speak
                                .iter()
                                .map(|c| c.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    }
                ));
            }
            continue;
        }
        if let Some(why) = unavailable(&snap, (&all, &mine), f, name) {
            let choices: Vec<&str> = mine
                .choices(f.tool())
                .iter()
                .map(|c| c.name.as_str())
                .collect();
            return Err(format!(
                "media default {} '{name}': {why}; pick one of: {}",
                f.as_str(),
                if choices.is_empty() {
                    "(none right now)".to_string()
                } else {
                    choices.join(", ")
                }
            ));
        }
    }
    Ok(new)
}
