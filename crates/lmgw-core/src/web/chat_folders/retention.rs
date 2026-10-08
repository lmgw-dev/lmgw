//! A folder's own retention (client-apps design §11 Q2): the purge days a
//! thread's `purge_at` is computed with — its folder's own when the folder
//! has them, else the global `chat_purge_days`. The sweep applies the same
//! rule in SQL (`store::sweep_chat_threads`).

use std::collections::HashMap;

use crate::state::AppState;
use crate::store::{self, ChatThread};

/// The purge days in effect: the global setting, and each folder's own.
#[derive(Debug, Clone, Default)]
pub(crate) struct PurgeDays {
    global: i64,
    folders: HashMap<i64, i64>,
}

impl PurgeDays {
    /// As stored now. A folder list that cannot be read leaves every thread
    /// on the global setting, with a log line.
    pub(crate) async fn load(state: &AppState) -> Self {
        let folders = match store::folder_purge_days(&state.db).await {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("chat: folders' own purge days not read, the global one shown: {e}");
                HashMap::new()
            }
        };
        Self {
            global: state.snapshot().settings.chat_purge_days,
            folders,
        }
    }

    /// Fixed days, for tests: `global`, and each folder's own.
    #[cfg(test)]
    pub(crate) fn fixed(global: i64, folders: &[(i64, i64)]) -> Self {
        Self {
            global,
            folders: folders.iter().copied().collect(),
        }
    }

    /// The days after archiving `t` is deleted (`0`: never).
    pub(crate) fn of(&self, t: &ChatThread) -> i64 {
        t.folder_id
            .and_then(|f| self.folders.get(&f).copied())
            .unwrap_or(self.global)
    }
}
