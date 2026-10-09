//! The Chat change feed (client-apps design §2): `GET /chat/api/feed`, one
//! SSE stream per client, carrying what changed in the Chat — threads and
//! folders created, updated and deleted (stored, resumable by cursor) — and
//! what is happening now — turns, bound voice sessions, the GPU hold (live).
//!
//! - **Stored** (`store::feed`): every thread and folder write records a
//!   change in its own transaction; [`Feed::wake`] tells the open streams
//!   to read the table once the write committed. Delivery renders the
//!   thread or folder as it is then (`render`).
//! - **Live** (`live`): turns and bound sessions register in the live half
//!   as they start, and publish their end as they drop; the hold is
//!   published when a snapshot changes it.
//! - **One stream** (`stream`): `hello`, the catch-up, then both kinds in
//!   order, keep-alive comments, and `revoked` when the key is.
//! - **The dashboard** (`dashboard`): `/api/events`' `chat` frame reads the
//!   same table on the same wake, for the owner, and names the threads
//!   whose messages changed from the message marks (`marks`), which the
//!   table does not hold yet.
//!
//! Who sees what is decided at delivery: a device never receives an event
//! about an Admin Chat thread, stored or live, in the catch-up, `hello` or
//! `state` (L3).

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::Sse;
use axum::response::{IntoResponse, Response};
use axum::Extension;
use futures::StreamExt;
use lmgw_api_types::chat_feed::{event, Cursor, FeedPrincipal, Hello};
use serde::Deserialize;
use std::collections::VecDeque;
use std::convert::Infallible;
use tokio::sync::watch;

use super::chat::err_json;
use super::chat_caller::Caller;
use super::chat_extract::ChatQuery;
use crate::config::{ApiKeyKind, Snapshot};
use crate::devices::LinkKind;
use crate::principal::Principal;
use crate::proxy::RequestCtx;
use crate::state::SharedState;
use crate::store::feed as table;

mod approvals;
pub(super) mod dashboard;
mod devices;
mod live;
mod marks;
mod opening;
mod render;
mod stream;
mod tasks;

pub(crate) use live::{hold_of, Ending, LiveFeed, TurnObserver, TurnWatch, VoiceEnd, VoiceWatch};

/// The feed's in-process half, one per gateway (`AppState::chat_feed`).
pub struct Feed {
    /// Moves after every committed write that recorded a change: what the
    /// open streams wait on before they read the table.
    stored: watch::Sender<u64>,
    /// Turns, bound sessions and the hold.
    pub(crate) live: LiveFeed,
    /// The threads whose messages changed (`marks`): moved by every message
    /// write of a stored thread, read by the dashboard's `chat` frame.
    messages: watch::Sender<marks::Marks>,
}

impl Feed {
    /// The feed of a gateway whose published snapshot is `snap`.
    pub(crate) fn new(snap: &Snapshot) -> Self {
        Self {
            stored: watch::Sender::new(0),
            live: LiveFeed::new(
                snap.settings.chat_feed_live_buffer,
                hold_of(&snap.settings.hold),
            ),
            messages: watch::Sender::new(marks::Marks::default()),
        }
    }

    /// Thread `thread_id`'s messages changed: a write through the Chat's
    /// repository seam went through (`web::chat_repo`). A temporary thread
    /// is never in the feed (L7).
    pub(crate) fn messages_changed(&self, thread_id: i64) {
        if thread_id > 0 {
            self.messages.send_modify(|m| m.mark(thread_id));
        }
    }

    /// Stored threads `ids` were deleted: their message marks go, so they do
    /// not pile up for threads that are gone (`marks`). Nobody is woken:
    /// the delete's own record names each thread to every reader.
    pub(crate) fn threads_deleted(&self, ids: &[i64]) {
        if !ids.is_empty() {
            self.messages.send_if_modified(|m| {
                m.forget(ids);
                false
            });
        }
    }

    /// The threads whose messages were marked after generation `seen`, and
    /// the generation now (tests).
    #[cfg(test)]
    pub(crate) fn marked_since(&self, seen: u64) -> (Vec<i64>, u64) {
        self.messages.borrow().since(seen)
    }

    /// A write that recorded a change has committed: every open stream reads
    /// the table. A stream also reads it at each keep-alive tick, so a write
    /// path that forgot to call this delays its event by at most that long.
    pub(crate) fn wake(&self) {
        self.stored.send_modify(|n| *n = n.wrapping_add(1));
    }

    /// A snapshot was published (`AppState::publish_snapshot` and every
    /// other swap): its hold and the live buffer's size.
    ///
    /// `current` reads the snapshot published now, under the live half's
    /// lock (review W4-23): whichever publish runs last applies the newest.
    pub(crate) fn published(&self, current: impl FnOnce() -> std::sync::Arc<Snapshot>) {
        self.live.published_from(|| {
            let snap = current();
            (
                hold_of(&snap.settings.hold),
                snap.settings.chat_feed_live_buffer,
            )
        });
    }
}

/// `GET /chat/api/feed`'s query.
#[derive(Deserialize, Default, schemars::JsonSchema)]
pub(crate) struct FeedQuery {
    /// Resume after this cursor (`"<epoch>:<seq>:<tag>"`). A `Last-Event-ID`
    /// header wins over it: a browser's `EventSource` reconnects with the
    /// URL it opened, so its `since` is the old one and the header the new.
    #[serde(default)]
    since: Option<String>,
}

/// `GET /chat/api/feed[?since=<cursor>]` (§2.1): the change feed as SSE, for
/// the owner and for a paired device. Without a cursor it starts now.
///
/// Fails closed (review W4-9): the Chat API's own [`Caller`] extractor
/// refuses a request without a resolved principal (500) or with an
/// anonymous one (401), and the request's context is required, never a
/// default standing in for the owner.
pub(super) async fn feed(
    State(state): State<SharedState>,
    caller: Caller,
    Extension(ctx): Extension<RequestCtx>,
    headers: HeaderMap,
    ChatQuery(q): ChatQuery<FeedQuery>,
) -> Response {
    let given = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| q.since.filter(|s| !s.trim().is_empty()));
    let cursor = match given.as_deref().map(|s| (s, Cursor::parse(s))) {
        None => None,
        Some((_, Some(c))) => Some(c),
        Some((s, None)) => {
            return err_json(
                StatusCode::BAD_REQUEST,
                "bad_request",
                format!(
                    "'{s}' is not a feed cursor: a cursor is \"<epoch>:<seq>:<tag>\", as an \
                     event's id and hello.cursor give it"
                ),
            );
        }
    };
    match opening::frames(&state, &caller, &ctx, cursor).await {
        Ok(frames) => {
            let frames = frames.map(Ok::<_, Infallible>);
            // Ends when the server stops, without an event: the client's
            // reconnect finds the next one, or none.
            let frames = crate::server::until_stopped(&state.stops, ctx.served_at, frames, None);
            Sse::new(frames).into_response()
        }
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

/// Where the table and a device's levels stood when a feed was opened
/// (`table::bounds_and_switch`): the wake subscribed first, then the epoch,
/// then the head with the levels in one transaction, so nothing committed
/// after this moment is missed.
struct Read {
    stored: watch::Receiver<u64>,
    epoch: String,
    bounds: table::Bounds,
    levels: table::Levels,
}

impl Read {
    /// Subscribe to the table's wake, then read the epoch, the head and
    /// device `key_id`'s levels (`None`: the owner's, which are none).
    async fn now(
        state: &SharedState,
        key_id: Option<i64>,
    ) -> Result<Self, crate::error::GatewayError> {
        let mut stored = state.chat_feed.stored.subscribe();
        stored.borrow_and_update();
        let epoch = table::epoch(&state.db).await?;
        let (bounds, levels) = table::bounds_and_switch(&state.db, key_id).await?;
        Ok(Self {
            stored,
            epoch,
            bounds,
            levels,
        })
    }

    /// [`Self::now`] again with the same wake: the head and the levels as
    /// they are now.
    async fn again(
        &mut self,
        state: &SharedState,
        key_id: i64,
    ) -> Result<(), crate::error::GatewayError> {
        let (bounds, levels) = table::bounds_and_switch(&state.db, Some(key_id)).await?;
        self.bounds = bounds;
        self.levels = levels;
        Ok(())
    }

    /// Whether the published snapshot says the levels read for device
    /// `key_id` (a key the snapshot no longer holds is its revocation's).
    fn published(&self, state: &SharedState, key_id: i64) -> bool {
        let snap = state.snapshot();
        snap.settings.self_admin == self.levels.gateway
            && snap
                .api_keys
                .iter()
                .find(|k| k.id == key_id)
                .is_none_or(|k| k.self_admin == self.levels.device)
    }
}

/// Open one client's stream at where the table stood (`read`): `hello`,
/// then the `resync` its cursor may owe.
///
/// **A device's reach** (its admin-tools switch, reviews P-1 and P-5): the
/// switch is read in the transaction that reads the table's head, so it is
/// the one in force there. `hello` and the live state are at that reach,
/// and so is every record the stream renders: the catch-up up to that head
/// renders each record with the narrower of the reach the device had when
/// it was written and the one it has now, and a switch record of its own in
/// the catch-up is a `resync` (the stream's module doc). A device's levels
/// are the ones the published snapshot says (`opening`).
async fn open(
    state: &SharedState,
    caller: &Caller,
    ctx: &RequestCtx,
    cursor: Option<Cursor>,
    read: Read,
) -> Result<stream::FeedStream, crate::error::GatewayError> {
    let snap = state.snapshot();
    let settings = &snap.settings;
    let Read {
        stored,
        epoch,
        bounds,
        levels,
    } = read;
    let key_id = key_of(caller);
    // The reach now: a device's from its row and the gateway's level, read
    // with the head (what its admin tools may do, 2026-10-07).
    let admin = match key_id {
        Some(_) => levels.reach(),
        None => caller.reach(&snap),
    };
    let (live, now) = state.chat_feed.live.subscribe(admin);
    let said = levels.may_do();
    // The record the cursor names, for its check (review W5-4).
    let named = match cursor.as_ref().filter(|c| c.epoch == epoch && c.seq > 0) {
        Some(c) => table::record_at(&state.db, c.seq).await?,
        None => None,
    };
    let start = stream::start(
        cursor.as_ref(),
        &epoch,
        bounds,
        named.as_ref(),
        settings.chat_feed_retention_days,
    );
    // The levels the device had where the catch-up starts: each before its
    // first record up to the head, else the one it has now.
    let played_levels = match key_id {
        Some(id) => table::Levels {
            device: table::first_device_reach_after(&state.db, id, start.cursor, bounds.head)
                .await?
                .unwrap_or(levels.device),
            gateway: table::first_gateway_reach_after(&state.db, start.cursor, bounds.head)
                .await?
                .unwrap_or(levels.gateway),
        },
        None => levels,
    };
    let played = match key_id {
        Some(_) => played_levels.reach(),
        None => admin,
    };
    // The cursor the stream continues from, with its record's check.
    let tag = match &named {
        Some(r) if r.seq == start.cursor => Some(r.tag.clone()),
        _ => stream::tag_at(state, start.cursor).await?,
    };
    let (principal, hosts_label) = principal_of(&ctx.principal, &snap);
    let hello = Hello {
        epoch: epoch.clone(),
        cursor: Cursor {
            epoch: epoch.clone(),
            seq: start.cursor,
            tag: tag.clone(),
        }
        .to_string(),
        keepalive_s: settings.chat_feed_keepalive_s,
        retention_days: settings.chat_feed_retention_days,
        principal,
        hosts_label,
        // As it is now, the one the stream starts at.
        self_admin: said.into(),
        hold: now.hold,
        voice: now.voice,
        turns: now.turns,
    };
    let mut out = VecDeque::from([stream::frame(event::HELLO, &hello)]);
    if let Some(reason) = start.resync {
        out.push_back(stream::resync(reason));
        // The profile list follows every `resync` (personality-profiles
        // design §3.2).
        out.extend(render::profile_list(state));
    }
    // Counted as the device's feed link, `last_seen_at` stamped; watched
    // for revocation for every key (§1.6).
    let conn =
        crate::devices::connect(state, &ctx.principal, ctx.revocation_mark, LinkKind::Feed).await;
    Ok(stream::FeedStream {
        state: state.clone(),
        admin,
        levels,
        said,
        played,
        played_levels,
        caught_up_at: bounds.head,
        key_id,
        epoch,
        cursor: start.cursor,
        cursor_tag: tag,
        sent: start.cursor,
        page_size: settings.chat_feed_page_size.max(1),
        stored,
        live,
        keepalive: stream::FeedStream::keepalive(settings.chat_feed_keepalive_s),
        conn,
        out,
        pending: VecDeque::new(),
        dirty: true,
        ended: false,
        stalled: None,
        waiting: None,
    })
}

/// The device key a feed reads for; `None` for the owner.
fn key_of(caller: &Caller) -> Option<i64> {
    caller.is_device().then(|| caller.key_id()).flatten()
}

/// Who `hello` says the feed is for, and the device's hosting label.
fn principal_of(p: &Principal, snap: &Snapshot) -> (FeedPrincipal, Option<String>) {
    match p {
        Principal::Key {
            id,
            kind: ApiKeyKind::Device,
            name,
            ..
        } => (
            FeedPrincipal {
                kind: "device".into(),
                name: crate::devices::short_name(name).to_string(),
            },
            snap.api_keys
                .iter()
                .find(|k| k.id == *id)
                .and_then(|k| k.hosts_label.clone()),
        ),
        Principal::Key {
            kind: ApiKeyKind::Owner,
            name,
            ..
        } => (
            FeedPrincipal {
                kind: "owner".into(),
                name: name.strip_prefix("owner:").unwrap_or(name).to_string(),
            },
            None,
        ),
        // The `Chat` gate lets only an owner or a device through; an
        // in-process caller with no principal is the gateway's own.
        Principal::Key { name, .. } => (
            FeedPrincipal {
                kind: "key".into(),
                name: name.clone(),
            },
            None,
        ),
        // The extractor refused an anonymous request before this.
        Principal::Anonymous => (
            FeedPrincipal {
                kind: "anonymous".into(),
                name: String::new(),
            },
            None,
        ),
    }
}
