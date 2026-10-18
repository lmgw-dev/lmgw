//! A stored `dev_url` outlives the service block it overrode only until the
//! manifest that dropped the block is written: `agent_dev_url_set` refuses to
//! set or clear one for an agent with no `run.service`, so the write that takes
//! the service away has to take the URL with it.

use crate::agents::{self, Agent};
use crate::state::SharedState;
use crate::store;

/// After a manifest write: clear the row's `dev_url` (and the note about a
/// cleared one) when the stored manifest declares no `run.service`.
pub(super) async fn drop_without_service(state: &SharedState, id: &str) -> Result<(), String> {
    let Some(row) = store::get_agent(&state.db, id)
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(());
    };
    if row.dev_url.is_none() {
        return Ok(());
    }
    // A row this build cannot parse keeps what it has: it cannot be asked.
    let Ok(agent) = Agent::from_row(row) else {
        return Ok(());
    };
    if agents::service::service_of(&agent).is_some() {
        return Ok(());
    }
    store::set_agent_dev_url(&state.db, id, None)
        .await
        .map_err(|e| e.to_string())?;
    agents::service::forget_cleared_dev_url(state, id).await;
    Ok(())
}
