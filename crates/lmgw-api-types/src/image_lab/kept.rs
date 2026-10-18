//! The Image lab on the lab store (labs-store design §5): what a group keeps
//! as its form, and what a run answers. A generation or an edit is one group
//! of one cell, kept before its request goes out; its run keeps the request
//! it sent and the route's
//! answer, each picture an output artifact named `lab-artifact:<id>` where
//! its `b64_json` was. An edit's picture and mask are the group's input
//! artifacts, which the stored form names by id, so a re-opened edit sends
//! them again without an upload.

use serde::{Deserialize, Serialize};

use super::{no_delimiters, num, text, ImageGenForm};
use crate::lab_store::{title_of, LabFileRef, LabGroupView};

/// The plan's variant of a generation: one cell asking
/// `/v1/images/generations`.
pub const GENERATE_VARIANT: &str = "generate";
/// The plan's variant of an edit: one cell asking `/v1/images/edits`.
pub const EDIT_VARIANT: &str = "edit";

/// A group's form: the page's form, and for an edit its picture and mask as
/// kept files. Also what an edit's `form` part takes, so a re-opened edit
/// names its kept picture instead of uploading it again; an `image` or
/// `mask` part of the upload stands in for the ref.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ImageLabForm {
    #[serde(flatten)]
    pub form: ImageGenForm,
    /// An edit's picture, kept; `null` for a generation.
    #[serde(default)]
    pub image: Option<LabFileRef>,
    /// An edit's mask, kept; `null` without one.
    #[serde(default)]
    pub mask: Option<LabFileRef>,
}

/// What `POST /image-lab/api/groups/{id}/run` answers once the request was
/// dispatched, whatever the route said (labs-store Decision 20): the kept
/// group with its run, the route's status, code and message and its answer
/// inside the run. Only the lab's own refusals (a group that is not there or
/// is answered already, a stored form that no longer builds a request) keep
/// their status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ImageLabAnswer {
    /// The group, its one run inside. A run the store could not keep is
    /// there with no `id` and its pictures still inline as `b64_json`.
    pub group: LabGroupView,
    /// Why the run was not kept (a full disk, a lock held too long). The
    /// answer is the route's all the same.
    pub store_error: Option<String>,
}

impl ImageGenForm {
    /// What the form says that does not read, without asking for a model or
    /// a prompt: what a saved prompt preset must not hold (a step count that
    /// is not a number, half a size, a field carrying sd.cpp's delimiters).
    pub fn check_fields(&self) -> Result<(), String> {
        self.size()?;
        self.extra_args()?;
        num::<i64>("n", &self.n)?;
        num::<i64>("output compression", &self.output_compression)?;
        if let Some(p) = text(&self.prompt) {
            no_delimiters("the prompt", p)?;
        }
        Ok(())
    }

    /// The group's title: the prompt's first line.
    pub fn title(&self) -> String {
        title_of(&self.prompt).unwrap_or_else(|| "(no prompt)".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_form_with_its_kept_files_reads_as_the_form_and_two_refs() {
        let f = ImageLabForm {
            form: ImageGenForm {
                model: "image/kontext".into(),
                prompt: "make it night".into(),
                ..Default::default()
            },
            image: Some(LabFileRef {
                id: 4,
                name: "in.png".into(),
            }),
            mask: None,
        };
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v["prompt"], "make it night", "the form's fields stay flat");
        assert_eq!(v["image"]["id"], 4);
        let back: ImageLabForm = serde_json::from_value(v).unwrap();
        assert_eq!(back, f);
        // A plain form is a group form with no files.
        let plain: ImageLabForm =
            serde_json::from_str(r#"{"model":"image/z","prompt":"a cat"}"#).unwrap();
        assert_eq!(plain.form.prompt, "a cat");
        assert!(plain.image.is_none() && plain.mask.is_none());
    }

    #[test]
    fn a_preset_needs_no_model_or_prompt_but_its_numbers_must_read() {
        assert!(ImageGenForm::default().check_fields().is_ok());
        let f = ImageGenForm {
            steps: "eight".into(),
            ..Default::default()
        };
        assert!(f.check_fields().unwrap_err().contains("steps"));
        let f = ImageGenForm {
            width: "512".into(),
            ..Default::default()
        };
        assert!(f.check_fields().is_err());
        let f = ImageGenForm {
            prompt: format!("x {}", super::super::EXTRA_CLOSE),
            ..Default::default()
        };
        assert!(f.check_fields().unwrap_err().contains("the prompt"));
        assert_eq!(
            ImageGenForm {
                prompt: "\n a cat \n on a mat".into(),
                ..Default::default()
            }
            .title(),
            "a cat"
        );
    }
}
