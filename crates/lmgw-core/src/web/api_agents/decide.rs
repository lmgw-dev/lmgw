//! The Run tab's batch shape with a question set (labs-store design §4): an
//! output that decides through a set gets its `decide` view, and while the
//! config names a usable set, the review's values are the set's question's
//! options with the fallback last instead of `config.categories`. Read from
//! the agent as it is now, as the rest of the shape is.

use lmgw_api_types as dto;

use super::batch_shape;
use crate::agents::batch::decide;
use crate::agents::{Agent, ToolSurface};
use crate::state::SharedState;

/// [`batch_shape`], with the output's question set read.
pub(super) async fn batch_shape_now(
    state: &SharedState,
    agent: &Agent,
    surface: Option<&ToolSurface>,
) -> Option<dto::AgentBatchShape> {
    let mut shape = batch_shape(agent, surface)?;
    let config = agent.effective_config();
    if let Some((view, values)) = decide::review(state, &agent.manifest, &config).await {
        if let Some(values) = values {
            for e in shape
                .editable
                .iter_mut()
                .filter(|e| e.field == view.question)
            {
                e.options = values.clone();
            }
        }
        shape.decide = Some(view);
    }
    Some(shape)
}
