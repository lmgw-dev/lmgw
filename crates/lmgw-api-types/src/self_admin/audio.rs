//! `lmgw__audio_catalog`.
//!
//! The tool takes the three actions of the `audio_catalog` op (a refresh, a
//! queued download) and a third of its own, `list`, which is what the
//! dashboard reads from `GET /api/audio/catalog`. So it answers three things
//! by its arguments, and [`AudioCatalogToolAnswer`] is an `#[serde(untagged)]`
//! wrapper of them (`Serialize` and `JsonSchema` only: a reader knows the
//! action it sent and reads that action's own type).

use serde::{Deserialize, Serialize};

use crate::{AudioCatalogInstall, AudioCatalogRefreshed, AudioFamily, WithNextStep};

/// `lmgw__audio_catalog`, by `action`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum AudioCatalogToolAnswer {
    /// `action=list`.
    List(Box<AudioCatalogList>),
    /// `action=refresh`.
    Refreshed(AudioCatalogRefreshed),
    /// `action=download`, with what to do once the files are there.
    Downloaded(Box<WithNextStep<AudioCatalogInstall>>),
}

/// `lmgw__audio_catalog action=list`: the cached catalog, narrowed by
/// `family` and `search`. Never goes to the network. It is `GET
/// /api/audio/catalog`'s `AudioCatalog` and a `message`; a family's long
/// `options` come only for the family `family` names, the full listing says
/// where to get them in `options_summary`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AudioCatalogList {
    /// RFC 3339 time of the cached fetch; absent while nothing was fetched on
    /// this machine.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(transform = crate::openapi_ext::non_null))]
    pub fetched_at: Option<String>,
    pub families: Vec<AudioFamily>,
    /// What the last refresh could not do. Absent when there is nothing.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// What the listing is, or what to call first, in a sentence.
    pub message: String,
}
