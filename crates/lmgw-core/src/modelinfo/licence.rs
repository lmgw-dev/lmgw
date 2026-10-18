//! The licence a row's GGUF states, for the owner-side views (decision-models
//! design §3.9). Read through the shared header cache, so a warm page load
//! costs one `stat` per row.

use std::path::Path;

use lmgw_api_types as dto;

use crate::gguf::licence::Licence;
use crate::state::SharedState;

impl From<&Licence> for dto::ModelLicence {
    fn from(l: &Licence) -> Self {
        Self {
            id: l.id.clone(),
            name: l.name.clone(),
            link: l.link.clone(),
        }
    }
}

/// The licence of the GGUF at `rel` under `models_dir`. `None` when the
/// header states none and when the file cannot be read: a view that is only
/// information has nothing to say about a file that is missing, and the row's
/// own checks already report that.
pub(crate) async fn of_file(
    state: &SharedState,
    models_dir: &str,
    rel: &str,
) -> Option<dto::ModelLicence> {
    if models_dir.trim().is_empty() || rel.is_empty() {
        return None;
    }
    let s = state
        .gguf_cache
        .summarize_cached(&Path::new(models_dir).join(rel))
        .await
        .ok()?;
    s.licence.as_ref().map(dto::ModelLicence::from)
}
