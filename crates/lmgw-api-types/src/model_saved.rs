//! What a save of a local model answers: `POST /api/op/local_model_set`,
//! `aux_model_set`, `audio_model_set`
//! and `image_model_set`, and the `lmgw__*` tools of the same names.
//!
//! One type per op. The actions of an op do not send the same keys, so a key
//! that only some of them send is an `Option` that is left out when it is
//! `None` (and never `null`: the schema says so with `non_null`). The refusal
//! of any action is an error answer instead of one of these.

use serde::{Deserialize, Serialize};

/// A write to a local chat model: `create`, `update` (also `enable` and
/// `disable`), `delete` or `duplicate`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LocalModelSaved {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    /// The row's id: the new row's for `create` and `duplicate`.
    pub id: i64,
    /// `create`, `update`: why the model may not load, from the checks of the
    /// save (empty when none fired). Absent for `delete` and `duplicate`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub warnings: Option<Vec<String>>,
    /// `create`, `update`: saved and loadable, but misbehaving under some
    /// input; kept apart from `warnings`, which are reasons it may not load.
    /// Absent for `delete` and `duplicate`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub advisories: Option<Vec<String>>,
    /// `update`: whether a container is still up on the configuration before
    /// this change (mid-request, or a stop that failed), so a test now would
    /// load that one. Absent for every other action.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub container_kept_previous_config: Option<bool>,
    /// What was done, for a person to read, with the next step.
    pub message: String,
}

/// A write to an aux (embedding, rerank or decision) model: `create`, `update` (also
/// `enable` and `disable`) or `delete`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AuxModelSaved {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    /// The row's id.
    pub id: i64,
    /// `create`, `update`: why the model may not load (empty when none fired).
    /// Absent for `delete`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub warnings: Option<Vec<String>>,
    /// `create`: the name clients request the model by. Absent for every
    /// other action.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub public_name: Option<String>,
    /// What was done, for a person to read, with the next step.
    pub message: String,
}

/// A write to an audio model: `create`, `update` (also `enable` and
/// `disable`) or `delete`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AudioModelSaved {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    /// The row's id.
    pub id: i64,
    /// `update`: whether the save reset the model's learned residency (the
    /// owner named `residency` in `clear` and one was learned). Absent for
    /// every other action.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub residency_reset: Option<bool>,
    /// What was done, for a person to read.
    pub message: String,
}

/// A write to an image (stable-diffusion.cpp) model: `create`, `update` (also
/// `enable` and `disable`) or `delete`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ImageModelSaved {
    /// Always `true`; a refusal is an error answer instead.
    pub ok: bool,
    /// The row's id.
    pub id: i64,
    /// `create`, `update`: why the model may not load (empty when none fired).
    /// Absent for `delete`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub warnings: Option<Vec<String>>,
    /// `create`: the name clients request the model by. Absent for every
    /// other action.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub public_name: Option<String>,
    /// `update`: whether the save dropped the learned peak because the files or
    /// a flag changed, so the figure no longer describes the pipeline. Absent
    /// for every other action.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub peak_reset: Option<bool>,
    /// `create`, `update`: the `podman run` line the saved row starts with (an
    /// empty text when the row is not found). Absent for `delete`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub command_line: Option<String>,
    /// What was done, for a person to read, with the next step.
    pub message: String,
}
