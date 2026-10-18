//! `GET /api/session`: the dashboard's load check, which asks who is signed
//! in (principals design §3.4).

use serde::{Deserialize, Serialize};

/// What `GET /api/session` answers: the current principal, as the dashboard's
/// load check reads it. Never a `401`: `authenticated` is `false` and the
/// other two keys are empty when nobody is signed in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionView {
    pub authenticated: bool,
    /// `key`, `agent`, `owner` or `device` (a paired device) — empty when
    /// nobody is signed in. Present and empty rather than absent, so the
    /// response has one shape.
    pub kind: String,
    /// The name of the API key in use (`owner:dashboard` for the dashboard's
    /// own), empty when nobody is signed in.
    pub name: String,
}
