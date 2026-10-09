//! The `device.revoked` record as a reader receives it (MCP Tasks design
//! §4.1): the deleted device as a principal, and who deleted it.
//!
//! The owner reads every one. A device reads one at a level it sees — the
//! feed showed it the deleted device's changes (`store::feed::devices`) —
//! or one that names it as the host of a task the deleted device started:
//! that call's `lmgw/caller` named the device to it already.

use lmgw_api_types::chat_feed::{DeviceRevoked, FeedPrincipal};
use serde_json::{json, Value};

use crate::store::feed::Record;
use crate::store::AdminThreads;

/// The data of `r` (a `device.revoked` record) for a reader that reaches as
/// far as `admin`, reading for device key `reader` (`None`: the owner);
/// `None` when the reader may not see it.
pub(super) fn render(r: &Record, admin: AdminThreads, reader: Option<i64>) -> Option<Value> {
    let facts = r.device_revoked()?;
    let hosts_one = reader.is_some_and(|k| facts.hosts.contains(&k));
    if admin.is_device() && !admin.sees(r.admin) && !hosts_one {
        return None;
    }
    Some(json!(DeviceRevoked {
        device: FeedPrincipal {
            kind: "device".into(),
            name: facts.name,
        },
        by: r.by.clone(),
    }))
}
