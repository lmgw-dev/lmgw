//! The `device.revoked` record (MCP Tasks design §4.1, client-apps design
//! §2.2): a paired device's key was deleted, written in the delete's own
//! transaction, so a client that runs work the device started (a host of
//! MCP tasks whose `lmgw/caller` named it) may cancel that work.
//!
//! **Who hears of it** names no device a reader was not shown:
//! - the record's level is the lowest of the stored records that carry the
//!   device's `by` — a reader that may see one of them was shown the device
//!   — and the owner's alone ([`OWNER_ONLY`](super::super::OWNER_ONLY))
//!   when the feed holds none;
//! - beside that, `detail.hosts` lists the device keys that host a task
//!   the device started that was still open — running, or cancelled with
//!   the cancel still owed to its server (`cancel_owed`), which the host
//!   may still be running: their `lmgw/caller` named it on the call,
//!   whatever the thread's level.
//!
//! Only a delete is recorded: a disabled or expired key comes back (its
//! tasks wait), a rotated one keeps its row and pairs again under its name.

use serde_json::{json, Value};
use sqlx::SqliteConnection;

use super::super::DbResult;
use super::{kind, Record};

/// What a `device.revoked` record names: the device's paired name (no
/// prefix) and the device keys that host a task it started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRevoked {
    pub name: String,
    pub hosts: Vec<i64>,
}

/// Device key `key_id`, named `name` (`device:<name>`), is being deleted in
/// this transaction, by `by`: record `device.revoked` at the level its
/// stored changes were shown at, naming the hosts of the tasks it started
/// that are still open (module doc). Run before the key's row goes.
pub async fn record_device_revoked(
    conn: &mut SqliteConnection,
    key_id: i64,
    name: &str,
    by: super::By<'_>,
) -> DbResult<()> {
    let short = crate::devices::short_name(name);
    let shown_as = lmgw_api_types::chat_feed::by_device(short);
    let admin: i64 =
        sqlx::query_scalar("SELECT COALESCE(MIN(admin), ?2) FROM chat_feed WHERE by = ?1")
            .bind(&shown_as)
            .bind(super::super::OWNER_ONLY)
            .fetch_one(&mut *conn)
            .await?;
    // Still open: running (`open`), or cancelled with the cancel still owed
    // to its server (`cancel_owed`) — the host may run it yet.
    let hosts: Vec<i64> = sqlx::query_scalar(
        "SELECT DISTINCT s.device_key_id FROM mcp_tasks t
           JOIN mcp_servers s ON s.id = t.server_id
          WHERE t.started_by = ?1 AND t.state IN (?3, ?4)
            AND s.device_key_id IS NOT NULL AND s.device_key_id <> ?2
          ORDER BY s.device_key_id",
    )
    .bind(name)
    .bind(key_id)
    .bind(super::super::mcp_tasks::OPEN)
    .bind(super::super::mcp_tasks::CANCEL_OWED)
    .fetch_all(&mut *conn)
    .await?;
    super::record(
        conn,
        super::Change {
            kind: kind::DEVICE_REVOKED,
            admin,
            by,
            detail: Some(json!({ "name": short, "hosts": hosts }).to_string()),
            ..Default::default()
        },
    )
    .await
}

impl Record {
    /// A `device.revoked` record's facts; `None` for any other record.
    pub fn device_revoked(&self) -> Option<DeviceRevoked> {
        if self.kind != kind::DEVICE_REVOKED {
            return None;
        }
        let v: Value = serde_json::from_str(self.detail.as_deref()?).ok()?;
        Some(DeviceRevoked {
            name: v.get("name")?.as_str()?.to_string(),
            hosts: v
                .get("hosts")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_i64).collect())
                .unwrap_or_default(),
        })
    }
}
