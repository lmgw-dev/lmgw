//! The open host links, one per device row (§5.3).
//!
//! **A second link from the same key takes over.** The newer link is
//! registered, and the older one is told to close with 4000 "another
//! connection of device '<name>' took over" — its link task cancels the
//! calls open on it and closes. The manager's row follows the newer link
//! from then on (`conn`): a late word from the older one changes nothing.
//!
//! The table is also how lmgw closes a link for its own reasons: the row
//! deleted (the grant cleared) or switched off on the MCP page.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::oneshot;

/// Why lmgw closes a link: the close code and its reason.
pub(super) type CloseOrder = (u16, String);

/// One registered link: its number, its device, and the order that closes
/// it.
struct Open {
    link: u64,
    device: LinkDevice,
    close: oneshot::Sender<CloseOrder>,
}

/// The device a link is of: its key, and how a close names it ("device
/// 'desktop'", `devices::who`).
#[derive(Clone)]
pub(super) struct LinkDevice {
    pub(super) key_id: i64,
    pub(super) who: String,
}

/// The open links, by `mcp_servers.id`.
#[derive(Default)]
pub(crate) struct HostLinks {
    open: Mutex<HashMap<i64, Open>>,
}

impl HostLinks {
    /// Register link number `link` of `device` for row `server_id`. An older
    /// link of the row is told to close with `takeover` (§5.3). Returns the
    /// receiver of this link's own close order.
    ///
    /// Link numbers rise in the order the manager's entry took them
    /// (`McpManager::link_opened`), and the entry follows the highest. Two
    /// links of one device opening at once can register here in the other
    /// order: a link whose number is below the one registered is the older
    /// of the two, so it is the one told to close — at once — and the newer
    /// stays, as the entry does. Without that both would die: the newer one
    /// closed as taken over, the older one refused by the entry.
    pub(super) fn open(
        &self,
        server_id: i64,
        link: u64,
        device: LinkDevice,
        takeover: CloseOrder,
    ) -> oneshot::Receiver<CloseOrder> {
        let (tx, rx) = oneshot::channel();
        let mut open = self.lock();
        if open.get(&server_id).is_some_and(|o| o.link > link) {
            let _ = tx.send(takeover);
            return rx;
        }
        let older = open.insert(
            server_id,
            Open {
                link,
                device,
                close: tx,
            },
        );
        drop(open);
        if let Some(older) = older {
            let _ = older.close.send(takeover);
        }
        rx
    }

    /// Link `link` of row `server_id` ended: forget it, unless a newer link
    /// took its place.
    pub(super) fn ended(&self, server_id: i64, link: u64) {
        let mut open = self.lock();
        if open.get(&server_id).is_some_and(|o| o.link == link) {
            open.remove(&server_id);
        }
    }

    /// Close row `server_id`'s link, if it has one, with `order`.
    pub(super) fn close(&self, server_id: i64, order: CloseOrder) {
        if let Some(open) = self.lock().remove(&server_id) {
            let _ = open.close.send(order);
        }
    }

    /// The rows with a link open now, and the device of each.
    pub(super) fn rows(&self) -> Vec<(i64, LinkDevice)> {
        self.lock()
            .iter()
            .map(|(id, o)| (*id, o.device.clone()))
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<i64, Open>> {
        self.open.lock().unwrap_or_else(|e| e.into_inner())
    }
}
