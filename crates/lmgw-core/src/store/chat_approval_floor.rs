//! The owner's approval floor, as stored (migration 0076; client-apps design
//! §6.6): a JSON list of `mcp_tools` entries on every thread and folder,
//! whose `require_approval` a device's later write may not go below. What
//! sets it and how it is compared is `web::chat_tool_write`'s `approval`
//! module; here is how it is read and written, and what a thread leaving a
//! folder takes with it.

use std::collections::HashSet;

use super::{DbResult, ThreadDefaults, ThreadMcp};

/// A stored floor. Hand-edited text that does not parse reads as none, as
/// `mcp_tools` does (its thread then attaches nothing from it either).
pub(super) fn from_column(text: &str) -> Vec<ThreadMcp> {
    serde_json::from_str(text).unwrap_or_default()
}

/// A floor as stored.
pub(super) fn to_column(floor: &[ThreadMcp]) -> String {
    serde_json::to_string(floor).unwrap_or_else(|_| "[]".to_string())
}

/// Threads `ids`, about to leave folder `folder_id` (a move, the folder's
/// delete keeping them): while in it, a label a thread has no floor and no
/// entry for is compared with the folder's own floor and defaults, so out
/// of it the thread keeps those rules in its own floor — leaving a folder
/// never lowers what a device may set. Only for the labels the thread's
/// floor does not name: one the owner set on the thread itself stays the
/// owner's. By label, not by what the label resolves to (the store has no
/// snapshot): a folder entry naming the same server as the thread's by its
/// other label is kept too, which only makes the floor stricter.
pub(super) async fn fold_folder_floor(
    conn: &mut sqlx::SqliteConnection,
    folder_id: i64,
    ids: &[i64],
) -> DbResult<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT defaults, approval_floor FROM chat_folders WHERE id = ?1")
            .bind(folder_id)
            .fetch_optional(&mut *conn)
            .await?;
    let Some((defaults, floor)) = row else {
        return Ok(());
    };
    let mut folder: Vec<ThreadMcp> = Vec::new();
    let entries = from_column(&floor).into_iter().chain(
        ThreadDefaults::from_stored(&defaults)
            .mcp_tools
            .unwrap_or_default(),
    );
    for e in entries {
        // The rule is what counts; which tools the entry let through is the
        // thread's own business.
        let e = ThreadMcp {
            server_label: e.server_label.trim().to_string(),
            allowed_tools: None,
            require_approval: e.require_approval,
        };
        if !folder.contains(&e) {
            folder.push(e);
        }
    }
    if folder.is_empty() {
        return Ok(());
    }
    for &id in ids {
        let Some(text) = sqlx::query_scalar::<_, String>(
            "SELECT approval_floor FROM chat_threads WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?
        else {
            continue;
        };
        let mut floor = from_column(&text);
        let named: HashSet<String> = floor
            .iter()
            .map(|e| e.server_label.trim().to_string())
            .collect();
        let before = floor.len();
        floor.extend(
            folder
                .iter()
                .filter(|e| !named.contains(&e.server_label))
                .cloned(),
        );
        if floor.len() == before {
            continue;
        }
        sqlx::query("UPDATE chat_threads SET approval_floor = ?2 WHERE id = ?1")
            .bind(id)
            .bind(to_column(&floor))
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}
