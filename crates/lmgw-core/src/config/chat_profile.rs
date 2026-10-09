//! Personality profiles, the config half (personality-profiles design §1):
//! the profile as the [`Snapshot`] holds it ([`ChatProfile`]), its stored
//! body ([`ProfileBody`]), the input rules a write applies, and the
//! refusals a write can end in ([`ProfileRefusal`]).
//!
//! The store reads and writes the `chat_profiles` table
//! (`store::chat_profiles`), and every profile is in the snapshot, so voice
//! resolution and prompt assembly read it synchronously and a profile edit
//! reaches a bound voice session the way any config change does (D10). The
//! wire shapes and the examples' text form are `lmgw-api-types`', shared
//! with the editor and the client.

use lmgw_api_types::chat_profiles as api;
pub use lmgw_api_types::chat_profiles::{
    examples_from_text, examples_to_text, field, Example, FolderRef, ProfileCreate, ProfileDeleted,
    ProfileDraft, ProfilePatch, ProfileVoice, Reasoning, UsedBy, BUILTIN_CONCISE, RESERVED_NAME,
};
use serde_json::{Map, Value};

use super::Snapshot;

#[cfg(test)]
mod tests;

/// A profile as the snapshot holds it: the stored row with a built-in's
/// absent fields filled in from its built-in text.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChatProfile {
    pub id: i64,
    pub name: String,
    /// The built-in profile this row is ([`BUILTIN_CONCISE`]); `None` for
    /// one of the owner's own.
    pub builtin: Option<String>,
    /// Empty: none (the thread's own prompt applies).
    pub persona: String,
    /// Empty: none.
    pub length_rule: String,
    pub examples: Vec<Example>,
    /// `None`: the generic voice block; `Some("")`: none; a text: verbatim.
    pub voice_block: Option<String>,
    /// `None`: inherit.
    pub reasoning: Option<Reasoning>,
    pub voice: ProfileVoice,
    /// The fields taken from the built-in text ([`field::FOLLOWABLE`]'s
    /// names); empty on the owner's own rows.
    pub follows_builtin: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl ChatProfile {
    /// The row `id`/`name`/`builtin`/`body` resolved: a built-in row's
    /// absent fields take `def`'s texts. A row naming a built-in this build
    /// does not know (`def` none) reads as the owner's own.
    pub fn resolve(
        id: i64,
        name: String,
        builtin: Option<String>,
        body: ProfileBody,
        def: Option<&BuiltinProfile>,
        created_at: String,
        updated_at: String,
    ) -> Self {
        let mut follows = Vec::new();
        let mut take = |name: &str, absent: bool| {
            if absent && def.is_some() {
                follows.push(name.to_string());
            }
        };
        take(field::PERSONA, body.persona.is_absent());
        take(field::LENGTH_RULE, body.length_rule.is_absent());
        take(field::EXAMPLES, body.examples.is_absent());
        take(field::VOICE_BLOCK, body.voice_block.is_absent());
        take(field::REASONING, body.reasoning.is_absent());
        let b = def.map(BuiltinProfile::values);
        Self {
            id,
            name,
            builtin,
            persona: body
                .persona
                .resolve(b.as_ref().map(|b| b.persona.clone()))
                .unwrap_or_default(),
            length_rule: body
                .length_rule
                .resolve(b.as_ref().map(|b| b.length_rule.clone()))
                .unwrap_or_default(),
            examples: body
                .examples
                .resolve(b.as_ref().map(|b| b.examples.clone()))
                .unwrap_or_default(),
            voice_block: body
                .voice_block
                .resolve(b.as_ref().map(|b| b.voice_block.clone())),
            reasoning: body.reasoning.resolve(b.as_ref().map(|b| b.reasoning)),
            voice: body.voice,
            follows_builtin: follows,
            created_at,
            updated_at,
        }
    }

    /// The content fields, as Preview, Test and Speak take them.
    pub fn draft(&self) -> ProfileDraft {
        ProfileDraft {
            persona: self.persona.clone(),
            length_rule: self.length_rule.clone(),
            examples: self.examples.clone(),
            voice_block: self.voice_block.clone(),
            reasoning: self.reasoning,
            voice: self.voice.clone(),
        }
    }

    /// The wire shape, with what uses it.
    pub fn to_wire(&self, used_by: UsedBy) -> api::Profile {
        api::Profile {
            id: self.id,
            name: self.name.clone(),
            builtin: self.builtin.clone(),
            persona: self.persona.clone(),
            length_rule: self.length_rule.clone(),
            examples: self.examples.clone(),
            voice_block: self.voice_block.clone(),
            reasoning: self.reasoning,
            voice: self.voice.clone(),
            follows_builtin: self.follows_builtin.clone(),
            used_by,
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
        }
    }
}

impl Snapshot {
    /// The profile `id`, when it exists. A thread whose `profile_id` names
    /// a row that is gone has no profile (§2.1).
    pub fn chat_profile(&self, id: i64) -> Option<&ChatProfile> {
        self.chat_profiles.iter().find(|p| p.id == id)
    }

    /// The profile named `name`, in any case ([`name_key`]).
    pub fn chat_profile_named(&self, name: &str) -> Option<&ChatProfile> {
        let key = name_key(name);
        self.chat_profiles.iter().find(|p| name_key(&p.name) == key)
    }
}

/// A built-in profile's texts (`store::chat_profiles::builtin`). They are
/// not stored: a built-in row follows them for every field its body leaves
/// absent.
#[derive(Debug, Clone, Copy)]
pub struct BuiltinProfile {
    /// The `builtin` column's value.
    pub key: &'static str,
    /// The name it is seeded and created again with.
    pub name: &'static str,
    pub persona: &'static str,
    pub length_rule: &'static str,
    /// `(user, reply)` pairs.
    pub examples: &'static [(&'static str, &'static str)],
    pub voice_block: Option<&'static str>,
    pub reasoning: Option<Reasoning>,
}

impl BuiltinProfile {
    /// The built-in's value of each field, normalised as a write is
    /// ([`normalise_draft`]), so "equal to the built-in" is one comparison.
    fn values(&self) -> Written {
        Written::of(&ProfileDraft {
            persona: self.persona.to_string(),
            length_rule: self.length_rule.to_string(),
            examples: self
                .examples
                .iter()
                .map(|(u, r)| Example {
                    user: u.to_string(),
                    reply: r.to_string(),
                })
                .collect(),
            voice_block: self.voice_block.map(str::to_string),
            reasoning: self.reasoning,
            voice: ProfileVoice::default(),
        })
    }
}

/// A content field as a body stores it.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Slot<T> {
    /// No key: unset on the owner's own row, the built-in value on a
    /// built-in one.
    #[default]
    Absent,
    /// An explicit `null`: unset on either.
    Unset,
    Set(T),
}

impl<T> Slot<T> {
    pub fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }

    /// The value in force: `builtin` is the built-in's value on a built-in
    /// row, `None` on the owner's own.
    fn resolve(self, builtin: Option<Option<T>>) -> Option<T> {
        match self {
            Self::Absent => builtin.flatten(),
            Self::Unset => None,
            Self::Set(v) => Some(v),
        }
    }
}

impl<T: PartialEq> Slot<T> {
    /// The slot a write of `value` stores (`None`: unset). On a built-in row
    /// (`builtin` its value) a write equal to the built-in stores absent,
    /// so the field follows the built-in again (the
    /// `set_default_chat_prompt` rule); on the owner's own row unset is
    /// stored absent.
    fn written(value: Option<T>, builtin: Option<Option<T>>) -> Self {
        match builtin {
            Some(b) if b == value => Self::Absent,
            Some(_) => value.map_or(Self::Unset, Self::Set),
            None => value.map_or(Self::Absent, Self::Set),
        }
    }
}

/// `chat_profiles.body`: the content fields, each absent, unset or set, and
/// the voice (absent fields of it unset). Strict on input (the API's
/// `deny_unknown_fields` types), read tolerantly: a key this build cannot
/// read is dropped on its own ([`Self::from_stored`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProfileBody {
    pub persona: Slot<String>,
    pub length_rule: Slot<String>,
    pub examples: Slot<Vec<Example>>,
    pub voice_block: Slot<String>,
    pub reasoning: Slot<Reasoning>,
    pub voice: ProfileVoice,
}

impl ProfileBody {
    /// Read the stored JSON. Text that is not an object is an empty body; a
    /// key that does not read as its field's type is absent, as is any key
    /// this build does not know (a newer build's).
    pub fn from_stored(text: &str) -> Self {
        let Ok(Value::Object(map)) = serde_json::from_str::<Value>(text) else {
            return Self::default();
        };
        fn slot<T: serde::de::DeserializeOwned>(map: &Map<String, Value>, key: &str) -> Slot<T> {
            match map.get(key) {
                None => Slot::Absent,
                Some(Value::Null) => Slot::Unset,
                Some(v) => serde_json::from_value(v.clone()).map_or(Slot::Absent, Slot::Set),
            }
        }
        let voice = match map.get("voice") {
            Some(Value::Object(v)) => {
                let text = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
                ProfileVoice {
                    tts_alias: text("tts_alias"),
                    voice: text("voice"),
                    speech_style: text("speech_style"),
                }
            }
            _ => ProfileVoice::default(),
        };
        Self {
            persona: slot(&map, field::PERSONA),
            length_rule: slot(&map, field::LENGTH_RULE),
            examples: slot(&map, field::EXAMPLES),
            voice_block: slot(&map, field::VOICE_BLOCK),
            reasoning: slot(&map, field::REASONING),
            voice,
        }
    }

    /// The stored JSON: an absent field has no key, an unset one `null`;
    /// the voice only with the fields it sets, and no key when it sets none.
    pub fn to_stored(&self) -> String {
        fn put<T: serde::Serialize>(map: &mut Map<String, Value>, key: &str, s: &Slot<T>) {
            match s {
                Slot::Absent => {}
                Slot::Unset => {
                    map.insert(key.into(), Value::Null);
                }
                Slot::Set(v) => {
                    map.insert(key.into(), serde_json::to_value(v).unwrap_or(Value::Null));
                }
            }
        }
        let mut map = Map::new();
        put(&mut map, field::PERSONA, &self.persona);
        put(&mut map, field::LENGTH_RULE, &self.length_rule);
        put(&mut map, field::EXAMPLES, &self.examples);
        put(&mut map, field::VOICE_BLOCK, &self.voice_block);
        put(&mut map, field::REASONING, &self.reasoning);
        let mut voice = Map::new();
        for (k, v) in [
            ("tts_alias", &self.voice.tts_alias),
            ("voice", &self.voice.voice),
            ("speech_style", &self.voice.speech_style),
        ] {
            if let Some(v) = v {
                voice.insert(k.into(), Value::String(v.clone()));
            }
        }
        if !voice.is_empty() {
            map.insert("voice".into(), Value::Object(voice));
        }
        Value::Object(map).to_string()
    }

    /// The body a create with `d` (normalised) stores; `def` is the
    /// built-in when the row is one.
    pub fn from_draft(d: &ProfileDraft, def: Option<&BuiltinProfile>) -> Self {
        let w = Written::of(d);
        let b = def.map(BuiltinProfile::values);
        Self {
            persona: Slot::written(w.persona, b.as_ref().map(|b| b.persona.clone())),
            length_rule: Slot::written(w.length_rule, b.as_ref().map(|b| b.length_rule.clone())),
            examples: Slot::written(w.examples, b.as_ref().map(|b| b.examples.clone())),
            voice_block: Slot::written(w.voice_block, b.as_ref().map(|b| b.voice_block.clone())),
            reasoning: Slot::written(w.reasoning, b.as_ref().map(|b| b.reasoning)),
            voice: d.voice.clone(),
        }
    }

    /// Lay a patch (normalised, [`normalise_patch`]) over this body; `def`
    /// is the built-in when the row is one. A field the patch does not name
    /// stays as stored.
    pub fn apply_patch(&mut self, p: &ProfilePatch, def: Option<&BuiltinProfile>) {
        let b = def.map(BuiltinProfile::values);
        if let Some(v) = &p.persona {
            let v = v.clone().filter(|s| !s.is_empty());
            self.persona = Slot::written(v, b.as_ref().map(|b| b.persona.clone()));
        }
        if let Some(v) = &p.length_rule {
            let v = v.clone().filter(|s| !s.is_empty());
            self.length_rule = Slot::written(v, b.as_ref().map(|b| b.length_rule.clone()));
        }
        if let Some(v) = &p.examples {
            let v = v.clone().filter(|e| !e.is_empty());
            self.examples = Slot::written(v, b.as_ref().map(|b| b.examples.clone()));
        }
        if let Some(v) = &p.voice_block {
            self.voice_block = Slot::written(v.clone(), b.as_ref().map(|b| b.voice_block.clone()));
        }
        if let Some(v) = &p.reasoning {
            self.reasoning = Slot::written(*v, b.as_ref().map(|b| b.reasoning));
        }
        if let Some(v) = &p.voice {
            self.voice = v.clone().unwrap_or_default();
        }
    }
}

/// A draft's fields as written: `None` for unset (an empty persona, length
/// rule or example list is none).
struct Written {
    persona: Option<String>,
    length_rule: Option<String>,
    examples: Option<Vec<Example>>,
    voice_block: Option<String>,
    reasoning: Option<Reasoning>,
}

impl Written {
    fn of(d: &ProfileDraft) -> Self {
        let text = |s: &str| Some(s.trim().to_string()).filter(|s| !s.is_empty());
        Self {
            persona: text(&d.persona),
            length_rule: text(&d.length_rule),
            examples: Some(d.examples.clone()).filter(|e| !e.is_empty()),
            voice_block: d.voice_block.as_deref().map(|s| s.trim().to_string()),
            reasoning: d.reasoning,
        }
    }
}

/// Why a profile write was refused, with the status and code the routes
/// answer it with (§1.1, §3.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileRefusal {
    /// A field that is wrong as given, said in the text.
    Invalid(String),
    /// The name is `default` in some case.
    NameReserved,
    /// Another profile has the name, in some case: its name.
    NameTaken(String),
    /// The built-in exists (create it again only after a delete).
    BuiltinExists(String),
    /// No built-in profile has this key.
    UnknownBuiltin(String),
    /// A reset to the built-in on a row that is none.
    NotBuiltin(i64),
    NotFound(i64),
    /// A device's change, reset or delete of a profile that Admin Chat or
    /// the self-admin toolset uses (threads, folder defaults), while the
    /// device may not use lmgw's admin tools with writes (review fix 1):
    /// the counts, never a title.
    InAdminUse {
        threads: i64,
        folders: i64,
    },
}

impl ProfileRefusal {
    pub fn status(&self) -> u16 {
        match self {
            Self::NameTaken(_) | Self::BuiltinExists(_) => 409,
            Self::NotFound(_) => 404,
            Self::InAdminUse { .. } => 403,
            Self::Invalid(_)
            | Self::NameReserved
            | Self::UnknownBuiltin(_)
            | Self::NotBuiltin(_) => 400,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "bad_request",
            Self::NameReserved => "profile_name_reserved",
            Self::NameTaken(_) => "profile_name_taken",
            Self::BuiltinExists(_) => "profile_builtin_exists",
            Self::UnknownBuiltin(_) => "unknown_builtin",
            Self::NotBuiltin(_) => "profile_not_builtin",
            Self::NotFound(_) => "not_found",
            Self::InAdminUse { .. } => "profile_in_admin_use",
        }
    }
}

impl std::fmt::Display for ProfileRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(m) => f.write_str(m),
            Self::NameReserved => write!(
                f,
                "\"{RESERVED_NAME}\" is reserved: it is no profile at all (a thread with \
                 profile_id null)"
            ),
            Self::NameTaken(n) => write!(f, "a profile named \"{n}\" exists already"),
            Self::BuiltinExists(k) => write!(
                f,
                "the built-in profile \"{k}\" exists; it can be created again only after it \
                 was deleted"
            ),
            Self::UnknownBuiltin(k) => write!(f, "there is no built-in profile \"{k}\""),
            Self::NotBuiltin(id) => write!(f, "profile {id} is not a built-in profile"),
            Self::NotFound(id) => write!(f, "no profile with id {id}"),
            Self::InAdminUse { threads, folders } => write!(
                f,
                "this profile steers lmgw's admin tools ({threads} thread(s) with Admin Chat or \
                 the self-admin toolset, {folders} folder default(s) with the toolset); a device \
                 may change, reset or delete it only while it may use lmgw's admin tools with \
                 writes (its own level and the gateway's self-admin level both 'full'). The \
                 owner can change it from the dashboard"
            ),
        }
    }
}

/// The key two names collide on: lowercased with Unicode's rules, so
/// "Ärger" and "ärger" are one name (SQLite's `NOCASE` folds ASCII only).
pub fn name_key(name: &str) -> String {
    name.trim().to_lowercase()
}

/// A name as written, trimmed: refused when empty or reserved.
pub fn normalise_name(name: &str) -> Result<String, ProfileRefusal> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ProfileRefusal::Invalid(
            "name: a profile needs a name".into(),
        ));
    }
    if name_key(name) == RESERVED_NAME {
        return Err(ProfileRefusal::NameReserved);
    }
    Ok(name.to_string())
}

/// Trim what was typed and refuse what is wrong: the texts are trimmed
/// (an empty persona or length rule is none, a voice block of spaces is
/// `""`, none), each example's sides are trimmed and must not be empty, and
/// the voice is normalised as a thread's ([`normalise_voice`]). No length
/// or count is bounded (D19).
pub fn normalise_draft(d: &mut ProfileDraft) -> Result<(), ProfileRefusal> {
    d.persona = d.persona.trim().to_string();
    d.length_rule = d.length_rule.trim().to_string();
    normalise_examples(&mut d.examples)?;
    if let Some(v) = d.voice_block.as_mut() {
        *v = v.trim().to_string();
    }
    normalise_voice(&mut d.voice);
    Ok(())
}

/// [`normalise_draft`] for the fields a patch names; the name too.
pub fn normalise_patch(p: &mut ProfilePatch) -> Result<(), ProfileRefusal> {
    if let Some(n) = p.name.as_mut() {
        *n = normalise_name(n)?;
    }
    for v in [
        p.persona.as_mut(),
        p.length_rule.as_mut(),
        p.voice_block.as_mut(),
    ]
    .into_iter()
    .flatten()
    .flatten()
    {
        *v = v.trim().to_string();
    }
    if let Some(Some(e)) = p.examples.as_mut() {
        normalise_examples(e)?;
    }
    if let Some(Some(v)) = p.voice.as_mut() {
        normalise_voice(v);
    }
    Ok(())
}

fn normalise_examples(examples: &mut [Example]) -> Result<(), ProfileRefusal> {
    for (i, e) in examples.iter_mut().enumerate() {
        e.user = e.user.trim().to_string();
        e.reply = e.reply.trim().to_string();
        for (side, text) in [("user", &e.user), ("reply", &e.reply)] {
            if text.is_empty() {
                return Err(ProfileRefusal::Invalid(format!(
                    "examples[{i}].{side}: an example's sides must not be empty"
                )));
            }
        }
    }
    Ok(())
}

/// The voice as a thread's is normalised (`store::ThreadVoice::normalise`):
/// the alias and the voice trimmed, empty is unset; the speech style
/// trimmed, and an empty one stays — it is "none". Whether the alias is a
/// text-to-speech model is the route's check, against the snapshot.
pub fn normalise_voice(v: &mut ProfileVoice) {
    for s in [&mut v.tts_alias, &mut v.voice] {
        *s = s
            .take()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
    }
    if let Some(s) = v.speech_style.as_mut() {
        *s = s.trim().to_string();
    }
}

/// What a create asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum CreateKind {
    /// The owner's own profile: its name (normalised) and content.
    Named(String, ProfileDraft),
    /// A built-in profile again, by key.
    Builtin(String),
}

/// A create's body read as one of the two kinds: a `builtin` key alone, or
/// a `name` with any content fields (normalised).
pub fn create_kind(c: &ProfileCreate) -> Result<CreateKind, ProfileRefusal> {
    if let Some(key) = &c.builtin {
        if c.name.is_some() || c.draft() != ProfileDraft::default() {
            return Err(ProfileRefusal::Invalid(
                "builtin: a built-in profile is created from its key alone, with its own name \
                 and texts"
                    .into(),
            ));
        }
        return Ok(CreateKind::Builtin(key.trim().to_string()));
    }
    let name = normalise_name(c.name.as_deref().unwrap_or(""))?;
    let mut d = c.draft();
    normalise_draft(&mut d)?;
    Ok(CreateKind::Named(name, d))
}
