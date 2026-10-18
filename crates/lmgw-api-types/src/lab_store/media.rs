//! What the Image and Audio labs keep beside the store's own shapes
//! (labs-store design §5): a kept file a form names by its artifact id, the
//! summary a multipart request is stored as (its text fields as sent, each
//! file part by name and size and, once kept, as `lab-artifact:<id>`), and a
//! group's title from a prompt or a text.

use serde::{Deserialize, Serialize};

/// How many characters of its first line a group's title keeps, then an
/// ellipsis: a label, the form keeps the whole text. The Decision lab's
/// rule, `decision_lab::form::TITLE_CHARS`.
pub use crate::decision_lab::form::TITLE_CHARS;

/// A file the store keeps, as a form names it: the artifact's id and the
/// name it came with. A re-opened form sends it back, so a re-ask uploads
/// nothing again.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LabFileRef {
    /// The artifact (`/labs/api/artifacts/{id}`).
    pub id: i64,
    /// The upload's file name.
    pub name: String,
}

/// One text field of a multipart request, as sent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LabMultipartField {
    pub name: String,
    pub value: String,
}

/// One file part of a multipart request, as sent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LabMultipartFile {
    /// The part's field name (`image`, `mask`, `file`).
    pub name: String,
    pub filename: String,
    /// The part's content type as the upload declared it; `null` if it
    /// declared none.
    #[serde(rename = "type")]
    pub content_type: Option<String>,
    /// The part's size in bytes.
    pub bytes: usize,
    /// The kept file, `lab-artifact:<id>`: the group's input artifact with
    /// these bytes. `null` when the store kept none.
    #[serde(default)]
    pub artifact: Option<String>,
}

/// What a multipart request sent, in place of a JSON body: its text fields
/// in the order written and each file part by name and size. The stored
/// `request` of a run whose request was an upload.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LabMultipartSummary {
    pub fields: Vec<LabMultipartField>,
    pub files: Vec<LabMultipartFile>,
}

impl LabMultipartSummary {
    /// The text field `name`'s value, the first one written.
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|f| f.name == name)
            .map(|f| f.value.as_str())
    }
}

/// A group's title from `text`: its first non-blank line, at most
/// [`TITLE_CHARS`] characters, then an ellipsis; `None` when every line is
/// blank.
pub fn title_of(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    if line.chars().count() <= TITLE_CHARS {
        return Some(line.to_string());
    }
    let mut t: String = line.chars().take(TITLE_CHARS).collect();
    t.push('…');
    Some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_is_the_first_line_cut_with_an_ellipsis() {
        assert_eq!(title_of("\n  a cat  \nrest").as_deref(), Some("a cat"));
        assert_eq!(title_of(" \n\t"), None);
        let long = "x".repeat(TITLE_CHARS + 3);
        let t = title_of(&long).unwrap();
        assert_eq!(t.chars().count(), TITLE_CHARS + 1);
        assert!(t.ends_with('…'));
    }

    #[test]
    fn a_file_part_is_named_type_on_the_wire_and_keeps_its_ref() {
        let s = LabMultipartSummary {
            fields: vec![LabMultipartField {
                name: "model".into(),
                value: "audio/asr".into(),
            }],
            files: vec![LabMultipartFile {
                name: "file".into(),
                filename: "a.wav".into(),
                content_type: Some("audio/wav".into()),
                bytes: 44,
                artifact: Some("lab-artifact:3".into()),
            }],
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["files"][0]["type"], "audio/wav");
        assert_eq!(v["files"][0]["artifact"], "lab-artifact:3");
        assert_eq!(s.field("model"), Some("audio/asr"));
        let back: LabMultipartSummary = serde_json::from_value(v).unwrap();
        assert_eq!(back, s);
    }
}
