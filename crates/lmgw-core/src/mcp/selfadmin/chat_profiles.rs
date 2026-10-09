//! `lmgw__profiles`, `lmgw__profile_set` and `lmgw__profile_delete`
//! dispatch (personality-profiles design §3.4): the flat tool arguments
//! into what `ops::chat_profiles` takes, the same logic the
//! `/chat/api/profiles*` routes run. The examples arrive in their text
//! form; an empty or absent argument leaves a field as it is, and `clear`
//! unsets one. The voice block and the speech style each have a mode
//! (`…_mode`): their "none" is a stored `""`, which an empty argument
//! cannot say.

use lmgw_api_types::chat_profiles::{
    examples_from_text, examples_to_text, ProfileCreate, ProfilePatch, ProfileVoice, Reasoning,
};
use serde_json::{json, Map, Value};

use crate::ops;
use crate::ops::ProfileError;
use crate::state::SharedState;
use crate::store::{feed, AdminThreads};

use super::{arg_i64, arg_str, Caller};

/// The fields `clear` can unset.
const CLEARABLE: [&str; 6] = [
    "persona",
    "length_rule",
    "examples",
    "tts_alias",
    "voice",
    "speech_style",
];

/// A non-empty (after trimming) string argument; empty is "not given".
fn given<'a>(a: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>, String> {
    Ok(arg_str(a, key)?.filter(|s| !s.trim().is_empty()))
}

fn clears(a: &Map<String, Value>) -> Result<Vec<&str>, String> {
    let mut out = Vec::new();
    for f in arg_str(a, "clear")?
        .unwrap_or_default()
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|s| !s.is_empty())
    {
        let known = CLEARABLE
            .iter()
            .find(|k| **k == f)
            .ok_or_else(|| format!("clear: '{f}' is not one of {}", CLEARABLE.join(", ")))?;
        out.push(*known);
    }
    Ok(out)
}

/// `voice_block_mode` and `voice_block` as the profile's voice block:
/// `None` leaves it, `Some(None)` is the generic one, `Some(Some(text))`
/// the text (`""` for none).
fn voice_block(a: &Map<String, Value>) -> Result<Option<Option<String>>, String> {
    let text = given(a, "voice_block")?;
    match (
        arg_str(a, "voice_block_mode")?.filter(|s| !s.is_empty()),
        text,
    ) {
        (None, None) => Ok(None),
        (None | Some("own"), Some(t)) => Ok(Some(Some(t.to_string()))),
        (Some("own"), None) => Err("voice_block_mode 'own' needs voice_block".into()),
        (Some("generic"), None) => Ok(Some(None)),
        (Some("none"), None) => Ok(Some(Some(String::new()))),
        (Some("generic" | "none"), Some(_)) => {
            Err("voice_block is for voice_block_mode 'own' only".into())
        }
        (Some(m), _) => Err(format!(
            "voice_block_mode: '{m}' is not one of generic, none, own"
        )),
    }
}

/// `speech_style_mode` and `speech_style` as the profile's speech style
/// (profiles review fix 6): `None` leaves it, `Some(None)` inherits,
/// `Some(Some(text))` the text (`""` for none, D6's three-way rule as the
/// voice block's).
fn speech_style(a: &Map<String, Value>) -> Result<Option<Option<String>>, String> {
    let text = given(a, "speech_style")?;
    match (
        arg_str(a, "speech_style_mode")?.filter(|s| !s.is_empty()),
        text,
    ) {
        (None, None) => Ok(None),
        (None | Some("own"), Some(t)) => Ok(Some(Some(t.to_string()))),
        (Some("own"), None) => Err("speech_style_mode 'own' needs speech_style".into()),
        (Some("inherit"), None) => Ok(Some(None)),
        (Some("none"), None) => Ok(Some(Some(String::new()))),
        (Some("inherit" | "none"), Some(_)) => {
            Err("speech_style is for speech_style_mode 'own' only".into())
        }
        (Some(m), _) => Err(format!(
            "speech_style_mode: '{m}' is not one of inherit, none, own"
        )),
    }
}

/// `reasoning`: `None` leaves it, `Some(None)` inherits.
fn reasoning(a: &Map<String, Value>) -> Result<Option<Option<Reasoning>>, String> {
    match arg_str(a, "reasoning")?.filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(s) if s.eq_ignore_ascii_case("inherit") => Ok(Some(None)),
        Some(s) => Reasoning::parse(s)
            .map(|r| Some(Some(r)))
            .ok_or_else(|| format!("reasoning: '{s}' is not one of inherit, on, off")),
    }
}

/// Who a tool call is for the profile logic: how far it reaches, and the
/// name the feed event carries.
fn caller_view(state: &SharedState, caller: Caller) -> (AdminThreads, String) {
    let snap = state.snapshot();
    let admin = crate::devices::reach(&snap, caller.device);
    let by = match caller.device {
        None => feed::BY_OWNER.to_string(),
        Some(id) => snap.api_keys.iter().find(|k| k.id == id).map_or_else(
            || "a device".to_string(),
            |k| lmgw_api_types::chat_feed::by_device(crate::devices::short_name(&k.name)),
        ),
    };
    (admin, by)
}

fn refused(e: ProfileError) -> String {
    e.to_string()
}

pub(super) async fn run(
    state: &SharedState,
    name: &str,
    args: Option<Map<String, Value>>,
    caller: Caller,
) -> Result<Value, String> {
    let a = args.unwrap_or_default();
    let (admin, by) = caller_view(state, caller);
    match name {
        "lmgw__profiles" => {
            let list = ops::profiles_list(state, admin).await.map_err(refused)?;
            let mut v = serde_json::to_value(&list).map_err(|e| e.to_string())?;
            for (p, w) in list
                .profiles
                .iter()
                .zip(v["profiles"].as_array_mut().into_iter().flatten())
            {
                w["examples"] = json!(examples_to_text(&p.examples));
            }
            Ok(v)
        }
        "lmgw__profile_set" => {
            let action = arg_str(&a, "action")?.ok_or("action is required (create|update)")?;
            let clear = clears(&a)?;
            let block = voice_block(&a)?;
            let reasoning = reasoning(&a)?;
            let examples = match given(&a, "examples")? {
                Some(t) => Some(examples_from_text(t).map_err(|e| format!("examples: {e}"))?),
                None => None,
            };
            let (tts, voice, style) = (
                given(&a, "tts_alias")?,
                given(&a, "voice")?,
                speech_style(&a)?,
            );
            if style.is_some() && clear.contains(&"speech_style") {
                return Err(
                    "speech_style: name it in clear or give speech_style/speech_style_mode, \
                     not both"
                        .into(),
                );
            }
            let profile = match action {
                "create" => {
                    let mut c = ProfileCreate {
                        name: Some(
                            given(&a, "name")?
                                .ok_or("name is required to create a profile")?
                                .to_string(),
                        ),
                        persona: given(&a, "persona")?.unwrap_or_default().to_string(),
                        length_rule: given(&a, "length_rule")?.unwrap_or_default().to_string(),
                        examples: examples.unwrap_or_default(),
                        voice_block: block.flatten(),
                        reasoning: reasoning.flatten(),
                        voice: ProfileVoice {
                            tts_alias: tts.map(str::to_string),
                            voice: voice.map(str::to_string),
                            speech_style: style.clone().flatten(),
                        },
                        ..Default::default()
                    };
                    // A field named in `clear` is unset on create too.
                    for f in &clear {
                        match *f {
                            "persona" => c.persona.clear(),
                            "length_rule" => c.length_rule.clear(),
                            "examples" => c.examples.clear(),
                            "tts_alias" => c.voice.tts_alias = None,
                            "voice" => c.voice.voice = None,
                            _ => c.voice.speech_style = None,
                        }
                    }
                    ops::profile_create(state, &c, admin, Some(&by), caller.device).await
                }
                "update" => {
                    let id = arg_i64(&a, "id")?.ok_or("id is required to update a profile")?;
                    let mut p = ProfilePatch {
                        name: given(&a, "name")?.map(str::to_string),
                        persona: given(&a, "persona")?.map(|s| Some(s.to_string())),
                        length_rule: given(&a, "length_rule")?.map(|s| Some(s.to_string())),
                        examples: examples.map(Some),
                        voice_block: block,
                        reasoning,
                        voice: None,
                    };
                    if clear.contains(&"persona") {
                        p.persona = Some(None);
                    }
                    if clear.contains(&"length_rule") {
                        p.length_rule = Some(None);
                    }
                    if clear.contains(&"examples") {
                        p.examples = Some(None);
                    }
                    let voice_touched = tts.is_some()
                        || voice.is_some()
                        || style.is_some()
                        || clear
                            .iter()
                            .any(|f| f.ends_with("alias") || *f == "voice" || *f == "speech_style");
                    if voice_touched {
                        // The patch writes the whole voice object: start from the stored one.
                        let mut v = ops::profile_get(state, id, admin)
                            .await
                            .map_err(refused)?
                            .voice;
                        for (field, new) in [(&mut v.tts_alias, tts), (&mut v.voice, voice)] {
                            if let Some(n) = new {
                                *field = Some(n.to_string());
                            }
                        }
                        if let Some(s) = style {
                            v.speech_style = s;
                        }
                        for f in &clear {
                            match *f {
                                "tts_alias" => v.tts_alias = None,
                                "voice" => v.voice = None,
                                "speech_style" => v.speech_style = None,
                                _ => {}
                            }
                        }
                        p.voice = Some(Some(v));
                    }
                    ops::profile_update(state, id, p, admin, Some(&by), caller.device).await
                }
                other => return Err(format!("action '{other}' is not create or update")),
            }
            .map_err(refused)?;
            let mut v = serde_json::to_value(&profile).map_err(|e| e.to_string())?;
            v["examples"] = json!(examples_to_text(&profile.examples));
            Ok(v)
        }
        "lmgw__profile_delete" => {
            let id = arg_i64(&a, "id")?.ok_or("id is required")?;
            let d = ops::profile_delete(state, id, admin, Some(&by), caller.device)
                .await
                .map_err(refused)?;
            let mut v = serde_json::to_value(&d).map_err(|e| e.to_string())?;
            v["message"] = json!(format!(
                "profile {id} deleted; {} thread(s) and {} folder default(s) no longer use it",
                d.threads_cleared,
                d.folders_cleared.len()
            ));
            Ok(v)
        }
        other => Err(format!("unhandled built-in tool '{other}'")),
    }
}
