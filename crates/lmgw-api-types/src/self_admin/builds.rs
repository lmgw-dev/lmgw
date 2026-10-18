//! `lmgw__builds` and `lmgw__build_set`.
//!
//! The tools drive the Backends page's own ops (`builds`, `build_get`,
//! `build_set`), so most of what they answer is those ops' types
//! ([`BuildsResponse`], [`BuildGetResponse`], [`BuildSetResponse`]). What the
//! tools add is on top of them: the repository presets `lmgw__build_set
//! preset=` accepts, and the `next_step` after a save.
//!
//! `lmgw__builds` answers two different things by its arguments (every build
//! against one build and a page of its runs), so [`Builds`] is an
//! `#[serde(untagged)]` wrapper of the two (`Serialize` and `JsonSchema` only:
//! a reader knows the call it made and reads that call's own type).

use serde::{Deserialize, Serialize};

use crate::builds::{BuildGetResponse, BuildSetResponse, BuildsResponse, RepoPreset};

/// `lmgw__builds`: every build and the repository presets, or, with `id`, one
/// build and a page of its runs.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum Builds {
    /// No `id`: every build, and the presets.
    List(BuildList),
    /// `id` named: that build and a page of its runs.
    One(Box<BuildGetResponse>),
}

/// `lmgw__builds` without `id`: the `builds` op's answer, and the repository
/// presets: the `id` of one is what `lmgw__build_set preset=` takes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BuildList {
    #[serde(flatten)]
    pub builds: BuildsResponse,
    pub repo_presets: Vec<RepoPreset>,
}

/// `lmgw__build_set`: the `build_set` op's answer and, unless the build was
/// deleted, a `next_step`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BuildSet {
    #[serde(flatten)]
    pub answer: BuildSetResponse,
    /// What to call next. Absent after a `delete`, which leaves no build.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub next_step: Option<String>,
}
