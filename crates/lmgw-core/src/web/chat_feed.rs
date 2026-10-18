//! The Chat change feed (client-apps design §2): `GET /chat/api/feed`, one
//! SSE stream per client, carrying what changed in the Chat — threads,
//! folders and messages created, updated and deleted (stored, resumable by
//! cursor) — and what is happening now — turns, bound voice sessions, the
//! GPU hold (live).
//!
//! - **Stored** (`store::feed`): every thread, folder and message write
//!   records a change in its own transaction; [`Feed::wake`] tells the open
//!   streams to read the table once the write committed. Delivery renders
//!   the thread, folder or messages as they are then (`render`,
//!   `messages`).
//! - **Live** (`live`): turns and bound sessions register in the live half
//!   as they start, and publish their end as they drop; the hold is
//!   published when a snapshot changes it.
//! - **One stream** (`stream`): `hello`, the catch-up, `caught_up`, then
//!   both kinds in order, keep-alive comments at the stream's own interval
//!   (`keepalive`), and `revoked` when the key is.
//! - **For a device that was away** (device link design §7.3): `hello`
//!   lists the calls waiting for a decision at the head it opened at
//!   (`approvals`), and `caught_up` says when the catch-up up to that head
//!   is over.
//! - **The dashboard** (`dashboard`): `/api/events`' `chat` frame reads the
//!   same table on the same wake, for the owner. It names what the records
//!   name: the threads and folders that changed, and the threads whose
//!   messages changed (`message.*`).
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
mod keepalive;
mod live;
mod messages;
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
        }
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
    /// Seconds between this stream's keep-alive comments, from 1 to
    /// `chat_feed_keepalive_max_s` (Settings → Chat → Change feed → Longest
    /// keep-alive, default 900); a value outside is a 400 `bad_request`
    /// naming the bound and the setting. Without it the stream keeps the
    /// gateway's `chat_feed_keepalive_s`. `hello.keepalive_s` says the value
    /// the stream keeps. Only the comments follow it: stored events are
    /// read at the gateway's own interval whatever this one is. A phone
    /// holding its feed in the background asks a long one (about 120), so
    /// its radio wakes less.
    #[serde(default)]
    #[schemars(range(min = 1))]
    keepalive_s: Option<i64>,
}

/// `GET /chat/api/feed[?since=<cursor>][&keepalive_s=<n>]` (§2.1, device
/// link design §7.1): the change feed as SSE, for the owner and for a
/// paired device. Without a cursor it starts now.
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
    let keepalive_s = match keepalive::of_stream(&state.snapshot().settings, q.keepalive_s) {
        Ok(k) => k,
        Err(message) => return err_json(StatusCode::BAD_REQUEST, "bad_request", message),
    };
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
    match opening::frames(
        &state,
        &caller,
        &ctx,
        Asked {
            cursor,
            keepalive_s,
        },
    )
    .await
    {
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

/// What a feed's request asked for: where it resumes (`None`: now) and
/// the seconds between its keep-alive comments (`keepalive`).
struct Asked {
    cursor: Option<Cursor>,
    keepalive_s: u32,
}

/// Where the table and a device's levels stood when a feed was opened
/// (`table::opening`): the wake subscribed first, then the epoch, then the
/// head with the levels and the replies waiting for a decision in one
/// transaction, so nothing committed after this moment is missed and the
/// open approvals are the ones at the head.
struct Read {
    stored: watch::Receiver<u64>,
    epoch: String,
    bounds: table::Bounds,
    levels: table::Levels,
    /// The replies whose calls wait at `bounds.head`, for every reader:
    /// `hello` lists those in the reader's reach (`approvals::open`).
    waiting: Vec<table::WaitingReply>,
}

impl Read {
    /// Subscribe to the table's wake, then read the epoch, the head, device
    /// `key_id`'s levels (`None`: the owner's, which are none) and the
    /// waiting replies.
    async fn now(
        state: &SharedState,
        key_id: Option<i64>,
    ) -> Result<Self, crate::error::GatewayError> {
        let mut stored = state.chat_feed.stored.subscribe();
        stored.borrow_and_update();
        let epoch = table::epoch(&state.db).await?;
        let at = table::opening(&state.db, key_id).await?;
        Ok(Self {
            stored,
            epoch,
            bounds: at.bounds,
            levels: at.levels,
            waiting: at.waiting,
        })
    }

    /// [`Self::now`] again with the same wake: the head, the levels and the
    /// waiting replies as they are now.
    async fn again(
        &mut self,
        state: &SharedState,
        key_id: i64,
    ) -> Result<(), crate::error::GatewayError> {
        let at = table::opening(&state.db, Some(key_id)).await?;
        self.bounds = at.bounds;
        self.levels = at.levels;
        self.waiting = at.waiting;
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

/// Open one client's stream at where the table stood (`read`), from the
/// cursor it `asked` for and keeping a comment every keep-alive it asked
/// for (`keepalive`): `hello`, then the `resync` its cursor may owe;
/// `caught_up` follows once the stream reached the head (`stream`).
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
    Asked {
        cursor,
        keepalive_s,
    }: Asked,
    read: Read,
) -> Result<stream::FeedStream, crate::error::GatewayError> {
    let snap = state.snapshot();
    let settings = &snap.settings;
    let Read {
        stored,
        epoch,
        bounds,
        levels,
        waiting,
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
    // A device's row as the snapshot holds it: its URLs, certificate and
    // whether it is this computer's (device link design §7.2).
    let device = key_id.and_then(|id| snap.api_keys.iter().find(|k| k.id == id));
    let hello = Hello {
        epoch: epoch.clone(),
        cursor: Cursor {
            epoch: epoch.clone(),
            seq: start.cursor,
            tag: tag.clone(),
        }
        .to_string(),
        keepalive_s,
        retention_days: settings.chat_feed_retention_days,
        principal,
        hosts_label,
        // As it is now, the one the stream starts at.
        self_admin: said.into(),
        hold: now.hold,
        voice: now.voice,
        turns: now.turns,
        // At the head, in the reach now: the catch-up's `approval.*` are
        // already in it (device link design §7.3).
        open_approvals: approvals::open(&waiting, admin),
        urls: device.map(|k| k.link.urls.clone()).unwrap_or_default(),
        cert: device.and_then(|k| cert_of(&k.link)),
        dashboard_url: device.and_then(|k| {
            crate::net::dashboard_url(
                &settings.bind_addr,
                k.link.local_only,
                ctx.conn.as_ref().map(|c| c.local.ip()),
            )
        }),
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
    //
    // A device's feed sets `TCP_USER_TIMEOUT` on its connection's socket
    // from `keepalive_s` (§7.1, D16): a keep-alive that a vanished phone
    // never acknowledges then fails within one more interval and the stream
    // ends. `Links` records the keep-alive, the peer and the start.
    set_user_timeout(ctx, keepalive_s);
    let conn = crate::devices::connect_with(state, ctx, LinkKind::Feed, Some(keepalive_s)).await;
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
        keepalive: stream::FeedStream::keepalive(keepalive_s),
        backstop: stream::FeedStream::keepalive(settings.chat_feed_keepalive_s),
        told_caught_up: false,
        conn,
        out,
        pending: VecDeque::new(),
        queue: VecDeque::new(),
        purge: None,
        dirty: true,
        ended: false,
        stalled: None,
        waiting: None,
    })
}

/// `hello.cert` from a device row: its latest certificate's dates; `None`
/// before it has one.
fn cert_of(link: &crate::config::DeviceLink) -> Option<lmgw_api_types::chat_feed::HelloCert> {
    Some(lmgw_api_types::chat_feed::HelloCert {
        not_after: link.cert_not_after.clone()?,
        notice_after: link.cert_notice_after.clone()?,
    })
}

/// Set `TCP_USER_TIMEOUT` on a device feed's connection to its keep-alive
/// (device link design §7.1): data the peer never acknowledges ends the
/// connection one interval after the comment that could not be delivered.
/// Only the device listener's connections carry a socket handle; a failure
/// is logged, never a refusal (the stream's own timers still work).
#[cfg(any(target_os = "linux", target_os = "android"))]
fn set_user_timeout(ctx: &RequestCtx, keepalive_s: u32) {
    let Some(socket) = ctx.conn.as_ref().and_then(|c| c.socket.as_ref()) else {
        return;
    };
    let timeout = std::time::Duration::from_secs(u64::from(keepalive_s));
    if let Err(e) = socket.set_tcp_user_timeout(Some(timeout)) {
        tracing::warn!("device feed: TCP_USER_TIMEOUT not set: {e}");
    }
}

/// Where the option does not exist (socket2 has it on Linux and Android
/// here), the stream's own timers are all there is.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn set_user_timeout(_: &RequestCtx, _: u32) {}

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
