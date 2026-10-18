//! `lmgw__profiles`, `lmgw__profile_set` and `lmgw__profile_delete`.
//!
//! The tools run the same ops as `/chat/api/profiles*`, so their answers are
//! the routes' with two changes for a model that reads and writes text: a
//! profile's examples are the text form ([`examples_to_text`], the form
//! `lmgw__profile_set` takes them in) instead of a list, and a delete says in
//! a sentence what it cleared.

use serde::{Deserialize, Serialize};

use crate::chat_profiles::{
    examples_to_text, Profile, ProfileDeleted, ProfileList, ProfileVoice, Reasoning, UsedBy,
};

/// A [`Profile`] as the tools show it: the same keys, with `examples` in the
/// text form. `lmgw__profile_set` answers one, and `lmgw__profiles` lists
/// them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ToolProfile {
    pub id: i64,
    /// Unique in any case; never `default`.
    pub name: String,
    /// The built-in profile this row is (`concise`); `null` for one of the
    /// owner's own.
    pub builtin: Option<String>,
    /// Who the model is and how it sounds; empty for none.
    pub persona: String,
    /// How long replies are; empty for none.
    pub length_rule: String,
    /// The example exchanges as text: `User:` and `Reply:` lines, a blank line
    /// between exchanges, continuation lines indented by two spaces; empty for
    /// none.
    pub examples: String,
    /// The voice turn's block: `null` for the generic one, `""` for none.
    pub voice_block: Option<String>,
    /// Reasoning on or off; `null` inherits.
    pub reasoning: Option<Reasoning>,
    pub voice: ProfileVoice,
    /// The fields a built-in row takes from the built-in text.
    pub follows_builtin: Vec<String>,
    pub used_by: UsedBy,
    pub created_at: String,
    pub updated_at: String,
}

impl From<&Profile> for ToolProfile {
    fn from(p: &Profile) -> Self {
        let Profile {
            id,
            name,
            builtin,
            persona,
            length_rule,
            examples,
            voice_block,
            reasoning,
            voice,
            follows_builtin,
            used_by,
            created_at,
            updated_at,
        } = p.clone();
        Self {
            id,
            name,
            builtin,
            persona,
            length_rule,
            examples: examples_to_text(&examples),
            voice_block,
            reasoning,
            voice,
            follows_builtin,
            used_by,
            created_at,
            updated_at,
        }
    }
}

/// `lmgw__profiles`: [`ProfileList`] with each profile a [`ToolProfile`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ToolProfileList {
    /// In name order.
    pub profiles: Vec<ToolProfile>,
    /// The profile new threads start with, `null` for none.
    pub default_profile_id: Option<i64>,
}

impl From<&ProfileList> for ToolProfileList {
    fn from(l: &ProfileList) -> Self {
        // Every field, no `..`: a field `ProfileList` gains fails to compile
        // here instead of going missing from the tool's answer.
        let ProfileList {
            profiles,
            default_profile_id,
        } = l;
        Self {
            profiles: profiles.iter().map(ToolProfile::from).collect(),
            default_profile_id: *default_profile_id,
        }
    }
}

/// `lmgw__profile_delete`: what the delete cleared ([`ProfileDeleted`]), and a
/// sentence that says so.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ToolProfileDeleted {
    #[serde(flatten)]
    pub deleted: ProfileDeleted,
    /// `profile <id> deleted; <n> thread(s) and <m> folder default(s) no longer
    /// use it`.
    pub message: String,
}

impl From<ProfileDeleted> for ToolProfileDeleted {
    fn from(deleted: ProfileDeleted) -> Self {
        let message = format!(
            "profile {} deleted; {} thread(s) and {} folder default(s) no longer use it",
            deleted.deleted,
            deleted.threads_cleared,
            deleted.folders_cleared.len()
        );
        Self { deleted, message }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_profiles::{Example, FolderRef};
    use serde_json::{json, Value};

    fn profile() -> Profile {
        Profile {
            id: 4,
            name: "Terse".into(),
            persona: "You are curt.".into(),
            examples: vec![
                Example {
                    user: "Hi".into(),
                    reply: "Hello.".into(),
                },
                Example {
                    user: "Two\nlines".into(),
                    reply: "Ok.".into(),
                },
            ],
            reasoning: Some(Reasoning::Off),
            follows_builtin: vec!["persona".into()],
            ..Default::default()
        }
    }

    #[test]
    fn a_tool_profile_is_the_profile_with_its_examples_as_text() {
        let p = profile();
        let mut want = serde_json::to_value(&p).unwrap();
        want["examples"] = json!("User: Hi\nReply: Hello.\n\nUser: Two\n  lines\nReply: Ok.");
        let tool = ToolProfile::from(&p);
        assert_eq!(serde_json::to_value(&tool).unwrap(), want);
        let back: ToolProfile = serde_json::from_value(want).unwrap();
        assert_eq!(back, tool);
    }

    #[test]
    fn the_list_keeps_the_default_and_the_order() {
        let list = ProfileList {
            profiles: vec![profile(), Profile::default()],
            default_profile_id: Some(4),
        };
        let tool = ToolProfileList::from(&list);
        let v: Value = serde_json::to_value(&tool).unwrap();
        assert_eq!(v["default_profile_id"], 4);
        assert_eq!(v["profiles"][0]["id"], 4);
        assert_eq!(v["profiles"][1]["examples"], "");
    }

    #[test]
    fn a_delete_says_what_it_cleared() {
        let d = ProfileDeleted {
            deleted: 7,
            threads_cleared: 2,
            folders_cleared: vec![FolderRef {
                id: 1,
                name: "F".into(),
            }],
            default_cleared: true,
        };
        let tool = ToolProfileDeleted::from(d);
        let v = serde_json::to_value(&tool).unwrap();
        assert_eq!(
            v,
            json!({
                "deleted": 7, "threads_cleared": 2, "default_cleared": true,
                "folders_cleared": [{"id": 1, "name": "F"}],
                "message": "profile 7 deleted; 2 thread(s) and 1 folder default(s) no longer use it",
            })
        );
        assert_eq!(
            serde_json::from_value::<ToolProfileDeleted>(v).unwrap(),
            tool
        );
    }
}
