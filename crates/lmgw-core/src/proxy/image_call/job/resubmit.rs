//! The job's one resubmit (module doc of `job`): after a container death,
//! through the hold's recovery; after a restart the container answers for,
//! straight away. Both spend the same single retry, as
//! `vram::retry_dead_container` gives a synchronous request one.

use std::future::Future;

use crate::error::GatewayError;
use crate::vram::LocalHold;

use super::{Ended, Followed};

/// Run `attempt` (a submit and its poll), and once more after a loss.
pub(super) async fn once<F, Fut>(hold: &LocalHold, attempt: F) -> Result<Ended, GatewayError>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<Followed, GatewayError>>,
{
    let attempted = hold.attempt();
    match attempt().await {
        Ok(Followed::Ended(e)) => return Ok(e),
        Ok(Followed::Gone(status)) => tracing::warn!(
            "{} model '{}' no longer knows the job lmgw submitted (it answered {status}): it \
             restarted under the job; submitting it again",
            hold.class(),
            hold.model_id()
        ),
        Err(GatewayError::Transport(why)) => {
            if let Err(e) = hold.recover(attempted).await {
                // A candidate this request may not bring back is the gate's
                // to answer — by picking again — not a restart that failed.
                if matches!(e, GatewayError::CandidateLost { .. }) {
                    return Err(e);
                }
                return Err(GatewayError::Upstream {
                    status: 502,
                    provider_type: None,
                    message: format!(
                        "{} model '{}' stopped answering ({why}) and could not be restarted: {e}",
                        hold.class(),
                        hold.model_id()
                    ),
                });
            }
        }
        Err(e) => return Err(e),
    }
    match attempt().await {
        Ok(Followed::Ended(e)) => Ok(e),
        Ok(Followed::Gone(status)) => Err(GatewayError::Upstream {
            status: 502,
            provider_type: None,
            message: format!(
                "the image server lost the job again after it was submitted a second time (it \
                 answered {status} while the job was being followed)"
            ),
        }),
        Err(e) => {
            if matches!(e, GatewayError::Transport(_)) {
                // Twice in a row: whatever is wrong is not this container, so
                // the claim must not stamp the model as freshly used.
                hold.mark_failed();
            }
            Err(e)
        }
    }
}
