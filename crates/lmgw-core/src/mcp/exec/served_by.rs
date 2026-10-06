//! Which server a tool call's row names when no executor wrote it
//! (realtime-server-tools design §2.5): a call whose future a cancel
//! dropped. Its row names the server the executors' rows would have named —
//! a built-in toolset by its in-process name, a registered server by its
//! own — so the dropped call sits beside its finished siblings in Logs.

use super::{DOCS_SERVER, KB_SERVER, SELF_ADMIN_SERVER};
use crate::mcp::{kb, selfadmin};
use crate::state::SharedState;

/// The server of the exposed tool `name`; `builtin`: a built-in toolset
/// runs it in-process, as [`super::SplitExecutor`] routes it. `None` for a
/// registered server's tool the aggregate no longer knows (its server went
/// away meanwhile).
pub(crate) async fn server_of(state: &SharedState, name: &str, builtin: bool) -> Option<String> {
    if builtin {
        let server = if selfadmin::owns(name) {
            SELF_ADMIN_SERVER
        } else if kb::owns(name) {
            KB_SERVER
        } else {
            DOCS_SERVER
        };
        return Some(server.to_string());
    }
    let snap = state.snapshot();
    let (server_id, _) = state.mcp.aggregate(&snap).await.reverse.remove(name)?;
    snap.mcp_servers.get(&server_id).map(|s| s.name.clone())
}
