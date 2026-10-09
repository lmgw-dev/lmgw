//! One client's feed (client-apps design §2.1, §2.3, §2.4): `hello`, then a
//! catch-up from its cursor in pages, then stored and live events as they
//! happen, keep-alive comments between them, and `revoked` at the end when
//! its key is.
//!
//! **Stored events are read from the table**, never from memory: the
//! stream reads `seq > cursor` a page at a time whenever a write woke it
//! (`Feed::wake`), and again at every keep-alive tick in case a wake was
//! missed. A slow client falls behind and loses nothing; the page bounds
//! what one stream holds, not what it delivers.
//!
//! **Live events come through the broadcast** (`live`). Before one goes out
//! the table is read up to date, so a `turn.started` never precedes the
//! `thread.created` of its thread. A client that read slower than live
//! events happened, past `chat_feed_live_buffer`, gets a fresh `state` that
//! names the setting, and the stream resubscribes: never a silent drop.
//!
//! **L3:** a device's stream skips every record and live event about a
//! thread or folder it does not see — Admin Chat always, the self-admin
//! toolset's unless the device may use lmgw's admin tools. The gaps this
//! leaves in `seq` are accepted (§2.3, *changed during the build*): a gap
//! says that something a device may not see changed, never which thread or
//! what — the thread, message and attachment ids every device is handed are
//! one sequence across all kinds of thread already.
//!
//! **The device's admin-tools level** (2026-10-07; a level since the
//! pre-merge review's P-3). The stream's reach is its own: read from the
//! key's row when it opens, in the transaction that reads the table's head,
//! and moved by its key's `device.reach` record as the stream reads it, in
//! commit order. Raised above `off`, the threads and folders with the
//! self-admin toolset come to it as `folder.created`, `thread.created` and
//! the `thread.updated` of its threads in such a folder; set to `off`, they
//! go as `thread.deleted`, those threads' `thread.updated` (in no folder
//! now) and `folder.deleted` — a delete's order. A fresh `state`, which
//! says the level, follows every move, `read_only` to `full` and back
//! included.
//!
//! **What its admin tools may do** (2026-10-07). The device's level is
//! capped by the gateway's self-admin level, and both the reach and what
//! `hello` and `state` say follow the capped level ([`table::Levels`]): a
//! device at `read_only` under a gateway at `off` sees none of the toolset's
//! threads, as at `off`. The gateway's level is read with the head, and
//! moved by the `gateway.reach` records every device's stream reads in
//! commit order. Such a record plays as its own level record does where it
//! moves the capped level to or from `off` — the threads and folders come
//! or go, or, in the catch-up, a `resync` — and a fresh `state` follows
//! where the capped level moved at all; nothing where it did not.
//!
//! **A device that was away** (review P-1, decided by the owner 2026-10-07).
//! Its catch-up — the records up to the head the stream opened at — renders
//! each record with the narrower of two reaches: the one the device had
//! when the record was written, and the one it has now. A level record of
//! its own met there that moved its reach is not played back as a
//! transition: it is one `resync` at that point, and the client reloads what
//! it shows at the reach it has now. So a device never receives what it may not see now, nor what was
//! written while it was not allowed to see it.
//!
//! **Live frames** wait in `pending` for the table to be read up to date,
//! and are checked again as they go out (review P-4): an event about a
//! thread the reach no longer covers, and a `state` computed for another
//! reach, are dropped; the reach's move queued a fresh `state` of its own.
//!
//! **A level record waits for the snapshot** (the desktop client's live
//! check, 2026-10-07). A level's write commits its record, then publishes
//! the snapshot every route a device reads decides with — the folder's
//! current thread, the bind, the thread by id. A stream that read the
//! record in between told the device its threads went while those routes
//! still served them. Read after the head, a level record plays once the
//! published snapshot says its level ([`FeedStream::snapshot_behind`]); the
//! publish itself wakes the feed (`AppState::publish_over`), whatever the
//! writer does after it. A stream opened in that moment reads its levels
//! once the snapshot says them too, behind a keep-alive comment
//! (`super::opening`).

use std::collections::VecDeque;
use std::time::Duration;

use axum::response::sse::Event as SseEvent;
use futures::FutureExt;
use lmgw_api_types::chat_feed::{event, LiveState, Resync, Revoked};
use tokio::sync::{broadcast, watch};

use super::live::{Live, Now};
use crate::config::{DeviceAdmin, SelfAdmin};
use crate::devices::{DeviceConnection, RevokeReason};
use crate::state::SharedState;
use crate::store::{feed as table, AdminThreads};

/// Where a stream starts reading, and the `resync` it owes its client first,
/// when its cursor could not be honoured.
pub(super) struct Start {
    pub cursor: i64,
    pub resync: Option<String>,
}

/// A live frame waiting for the table to be read up to date (module doc).
pub(super) enum Pending {
    /// A live event: goes out while the reach still covers its level.
    Event(Live),
    /// A `state` computed for the reach `at`, saying the admin tools may do
    /// `level`: goes out while the reach is still the stream's and no later
    /// `state` said another level.
    State {
        at: AdminThreads,
        level: SelfAdmin,
        data: LiveState,
    },
}

/// A move of a device's levels a record says (module doc).
#[derive(Debug, Clone, Copy)]
pub(super) enum Moved {
    /// Its own key's level is now this one (`device.reach`).
    Device(DeviceAdmin),
    /// The gateway's self-admin level is now this one (`gateway.reach`).
    Gateway(SelfAdmin),
}

impl Moved {
    /// `levels` with this move applied.
    fn applied(self, levels: table::Levels) -> table::Levels {
        match self {
            Self::Device(device) => table::Levels { device, ..levels },
            Self::Gateway(gateway) => table::Levels { gateway, ..levels },
        }
    }
}

/// What the stream needs to run: built by the route (`super::open`).
pub(super) struct FeedStream {
    pub state: SharedState,
    /// How far the reader reaches into the threads and folders that drive
    /// the self-admin plane (L3) now: moved by its key's `device.reach`
    /// record, and every `gateway.reach` record, read after `caught_up_at`.
    pub admin: AdminThreads,
    /// The device's own admin-tools level and the gateway's now (the
    /// owner's own is `off`), moved by the same records: a device's reach
    /// and what `state` says follow the capped level.
    pub levels: table::Levels,
    /// What `hello`, or the last `state` queued, said the device's admin
    /// tools may do (module doc).
    pub said: SelfAdmin,
    /// The reach the device had when the catch-up record being read was
    /// written: moved by its key's `device.reach` record and the
    /// `gateway.reach` records up to `caught_up_at` (module doc). The
    /// owner's is `admin`.
    pub played: AdminThreads,
    /// The levels that gave `played`.
    pub played_levels: table::Levels,
    /// The table's head when the stream opened: the records up to it are
    /// the catch-up.
    pub caught_up_at: i64,
    /// The device key it reads for; `None` for the owner.
    pub key_id: Option<i64>,
    pub epoch: String,
    pub cursor: i64,
    /// The check of the record at `cursor` (review W5-4), for the
    /// keep-alive's `id:`; `None` at 0 or where the table no longer holds
    /// it.
    pub cursor_tag: Option<String>,
    /// The cursor the client was last handed (`hello.cursor`, a stored
    /// event's `id:`): behind `cursor` while a device's stream skips records
    /// it may not see (review W4-15).
    pub sent: i64,
    pub page_size: u32,
    pub stored: watch::Receiver<u64>,
    pub live: broadcast::Receiver<Live>,
    pub keepalive: tokio::time::Interval,
    pub conn: Option<DeviceConnection>,
    /// Frames ready to go out, in order.
    pub out: VecDeque<SseEvent>,
    /// Live frames waiting for the table to be read up to date first.
    pub pending: VecDeque<Pending>,
    /// The table may hold records past `cursor`.
    pub dirty: bool,
    pub ended: bool,
    /// A record that failed to render, and how many reads in a row it
    /// failed (review W5-16).
    pub stalled: Option<(i64, u32)>,
    /// A level record waiting for the published snapshot to say it, and
    /// since when ([`Self::snapshot_behind`]).
    pub waiting: Option<(i64, tokio::time::Instant)>,
}

/// How many reads in a row a record may fail to render before the stream
/// moves past it with a `resync` that names it (review W5-16). A failure
/// is a database read that failed; one that keeps failing is a record that
/// cannot render, which would otherwise stall the stream at it for good.
pub(super) const RENDER_ATTEMPTS: u32 = 3;

/// The check of the record at `seq` (review W5-4): `None` at 0, or where
/// the table no longer holds it.
pub(super) async fn tag_at(
    state: &SharedState,
    seq: i64,
) -> Result<Option<String>, crate::error::GatewayError> {
    if seq <= 0 {
        return Ok(None);
    }
    Ok(table::record_at(&state.db, seq).await?.map(|r| r.tag))
}

impl FeedStream {
    /// A keep-alive interval that first ticks one period from now.
    pub(super) fn keepalive(seconds: u32) -> tokio::time::Interval {
        let period = Duration::from_secs(u64::from(seconds.max(1)));
        let mut i = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        i.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        i
    }

    fn device(&self) -> bool {
        self.admin.is_device()
    }

    /// The next frame, or `None` once the stream ended.
    pub(super) async fn next(&mut self) -> Option<SseEvent> {
        loop {
            // A revoked key hears nothing more, not even a frame already
            // queued (§1.6).
            if !self.ended {
                if let Some(reason) = self.revoked_now() {
                    self.revoke(reason);
                }
            }
            if let Some(frame) = self.out.pop_front() {
                return Some(frame);
            }
            if self.ended {
                return None;
            }
            if self.dirty {
                self.read_page().await;
                continue;
            }
            if let Some(p) = self.pending.pop_front() {
                self.push_live(p);
                continue;
            }
            tokio::select! {
                biased;
                reason = crate::devices::revoked(self.conn.as_mut()) => self.revoke(reason),
                got = self.live.recv() => self.on_live(got),
                changed = self.stored.changed() => match changed {
                    Ok(()) => self.dirty = true,
                    // The gateway is going away.
                    Err(_) => self.ended = true,
                },
                _ = self.keepalive.tick() => {
                    // Carrying the cursor when it moved past what the client
                    // was handed (review W4-15): records it may not see do
                    // not hold its `Last-Event-ID` back, so a reconnect after
                    // a long stretch of them resumes without a `resync`. An
                    // `id:` with no data moves an `EventSource`'s
                    // `lastEventId` and dispatches nothing.
                    let mut tick = SseEvent::default().comment("keep-alive");
                    if self.cursor > self.sent {
                        tick = tick.id(self.cursor_text(self.cursor, self.cursor_tag.clone()));
                        self.sent = self.cursor;
                    }
                    self.out.push_back(tick);
                    // The backstop for a write that recorded and forgot to
                    // wake the feed: read the table at every tick.
                    self.dirty = true;
                }
            }
        }
    }

    /// `"<epoch>:<seq>:<tag>"`.
    fn cursor_text(&self, seq: i64, tag: Option<String>) -> String {
        lmgw_api_types::chat_feed::Cursor {
            epoch: self.epoch.clone(),
            seq,
            tag,
        }
        .to_string()
    }

    /// A revocation already raised, without waiting.
    fn revoked_now(&mut self) -> Option<RevokeReason> {
        let conn = self.conn.as_mut()?;
        conn.revoked().now_or_never()
    }

    /// `revoked {reason, message}`, then the end (§1.6, L18). Nothing
    /// queued goes out after it.
    fn revoke(&mut self, reason: RevokeReason) {
        let who = self
            .conn
            .as_ref()
            .map(|c| c.name().to_string())
            .unwrap_or_default();
        let message = reason.message(&who);
        tracing::info!("chat feed: {message}");
        self.out.clear();
        self.pending.clear();
        self.out.push_back(frame(
            event::REVOKED,
            &Revoked {
                reason: reason.as_str().to_string(),
                message,
                kind: reason.kind(self.conn.as_ref().is_some_and(|c| c.is_device())),
            },
        ));
        self.ended = true;
    }

    /// One page of the table after the cursor, rendered for this reader.
    ///
    /// Read with the table's bounds (review W4-4): when retention pruned
    /// past the cursor while this stream was behind, the client gets the
    /// `resync` it would have got at the door, and the stream goes on from
    /// the newest record. A record that cannot be rendered now (a read that
    /// failed, review W4-5) stops the page before it, the cursor stays, and
    /// the next wake or keep-alive tick reads it again.
    async fn read_page(&mut self) {
        let read = table::page_with_bounds(&self.state.db, self.cursor, self.page_size).await;
        let (page, bounds) = match read {
            Ok(p) => p,
            Err(e) => {
                // A failed read loses nothing: the cursor stays, and the
                // next wake or tick reads again.
                tracing::warn!("chat feed: reading after {} failed: {e}", self.cursor);
                self.dirty = false;
                return;
            }
        };
        if bounds.pruned_through > self.cursor {
            let days = self.state.snapshot().settings.chat_feed_retention_days;
            self.out
                .push_back(resync(pruned_reason(days, bounds.pruned_through)));
            self.resynced();
            // `sent` stays: the next keep-alive hands the client the new
            // cursor (review W4-15).
            let mut head = bounds.head;
            // The records it passes may hold its own switch: the reach is
            // read again with the head it goes on from (P-1). A failed read
            // keeps the narrower of the two it knows until the next one.
            if let Some(id) = self.key_id {
                match table::bounds_and_switch(&self.state.db, Some(id)).await {
                    Ok((b, levels)) => {
                        head = b.head;
                        self.reach_is(levels);
                    }
                    Err(e) => {
                        tracing::warn!("chat feed: reading the device's level failed: {e}");
                        self.reach_is(table::Levels {
                            device: DeviceAdmin::Off,
                            ..self.levels
                        });
                    }
                }
            }
            self.cursor = head;
            self.cursor_tag = tag_at(&self.state, head).await.ok().flatten();
            self.dirty = true;
            return;
        }
        self.dirty = page.len() >= self.page_size as usize;
        // Once per page, not per record (review W5-15): every `thread.*`
        // record renders its `purge_at` with it.
        let purge = if page.is_empty() {
            None
        } else {
            Some(super::super::chat_folders::retention::PurgeDays::load(&self.state).await)
        };
        for r in &page {
            let purge = purge.as_ref().expect("loaded for a page with records");
            // Its own key's level, or the gateway's: the toolset's threads
            // and folders come or go for it, or, in the catch-up, a
            // `resync` (module doc).
            if let Some(moved) = self.level_record(r) {
                if r.seq <= self.caught_up_at {
                    self.flipped_while_away(r, moved);
                } else {
                    if self.snapshot_behind(r, moved).await {
                        // Read again at the wake that follows the publish.
                        self.dirty = false;
                        return;
                    }
                    self.reach_moved(r, moved, purge).await;
                }
                continue;
            }
            let reach = self.reach_at(r.seq);
            match super::render::render(&self.state, r, (reach, self.key_id), purge).await {
                Ok(rendered) => {
                    self.stalled = None;
                    self.cursor = r.seq;
                    self.cursor_tag = Some(r.tag.clone());
                    if let Some((event, data)) = rendered {
                        self.sent = r.seq;
                        self.out.push_back(
                            SseEvent::default()
                                .id(self.cursor_text(r.seq, Some(r.tag.clone())))
                                .event(event)
                                .data(data.to_string()),
                        );
                    }
                }
                Err(e) => {
                    let tries = match self.stalled {
                        Some((seq, n)) if seq == r.seq => n + 1,
                        _ => 1,
                    };
                    if tries < RENDER_ATTEMPTS {
                        tracing::warn!(
                            "chat feed: record {} not rendered ({e}); the stream waits at it and \
                             reads it again at its next wake or keep-alive ({tries} of \
                             {RENDER_ATTEMPTS} reads)",
                            r.seq
                        );
                        self.stalled = Some((r.seq, tries));
                        self.dirty = false;
                        return;
                    }
                    // Said, never skipped silently: the client reloads, and
                    // the stream goes on after the record.
                    tracing::error!(
                        "chat feed: record {} could not be rendered in {RENDER_ATTEMPTS} reads \
                         ({e}); the stream moves past it with a resync",
                        r.seq
                    );
                    self.out.push_back(resync(format!(
                        "the change at {} could not be read in {RENDER_ATTEMPTS} attempts ({e}); \
                         reload what you show",
                        self.cursor_text(r.seq, Some(r.tag.clone()))
                    )));
                    self.resynced();
                    self.stalled = None;
                    self.cursor = r.seq;
                    self.cursor_tag = Some(r.tag.clone());
                    self.dirty = true;
                    return;
                }
            }
        }
    }

    /// Record `r` as a move of this device's levels: its own key's
    /// `device.reach`, or any `gateway.reach`. `None` for every other record,
    /// and on the owner's stream.
    fn level_record(&self, r: &table::Record) -> Option<Moved> {
        if !self.device() {
            return None;
        }
        if let Some(level) = r.gateway_reach() {
            return Some(Moved::Gateway(level));
        }
        match r.device_reach() {
            Some((key, level)) if self.key_id == Some(key) => Some(Moved::Device(level)),
            _ => None,
        }
    }

    /// Whether level record `r` (`moved`), read after the head, waits: the
    /// published snapshot does not say its level yet (module doc). The
    /// write commits the record before it publishes the snapshot, and the
    /// routes a device reads on what the record brings it — the folder's
    /// current thread, the bind, the thread by id — read the snapshot, so a
    /// `thread.deleted` played now would tell the device of a reach those
    /// routes do not have yet. Not when a later record of the same kind
    /// follows (the snapshot may say that one already), nor for a device key
    /// the snapshot no longer holds (its revocation ends the stream). For
    /// up to about two keep-alive periods (Settings → Chat → Change feed →
    /// Keep-alive): the wait is measured from the record's first read and
    /// checked again at the next wake or tick, so a snapshot still behind
    /// one keep-alive after the first read is found so at the tick after
    /// that, said in the log, and the record plays — the store is what the
    /// feed follows.
    async fn snapshot_behind(&mut self, r: &table::Record, moved: Moved) -> bool {
        let Some(key_id) = self.key_id else {
            return false;
        };
        let behind = {
            let snap = self.state.snapshot();
            match moved {
                Moved::Device(level) => snap
                    .api_keys
                    .iter()
                    .find(|k| k.id == key_id)
                    .is_some_and(|k| k.self_admin != level),
                Moved::Gateway(level) => snap.settings.self_admin != level,
            }
        };
        let later = if behind {
            let db = &self.state.db;
            match moved {
                Moved::Device(_) => table::first_device_reach_after(db, key_id, r.seq, i64::MAX)
                    .await
                    .map(|l| l.is_some()),
                Moved::Gateway(_) => table::first_gateway_reach_after(db, r.seq, i64::MAX)
                    .await
                    .map(|l| l.is_some()),
            }
        } else {
            Ok(false)
        };
        if !behind || matches!(later, Ok(true)) {
            self.waiting = None;
            return false;
        }
        let since = match self.waiting {
            Some((seq, at)) if seq == r.seq => at,
            _ => {
                let now = tokio::time::Instant::now();
                self.waiting = Some((r.seq, now));
                now
            }
        };
        let period = self.keepalive.period();
        if since.elapsed() < period {
            return true;
        }
        tracing::warn!(
            "chat feed: record {} moved a device's admin-tools level, and the published \
             snapshot still did not say it after the stream waited between one and two \
             keep-alives ({} s each, Settings → Chat → Change feed → Keep-alive); the stream \
             plays it as the store holds it",
            r.seq,
            period.as_secs()
        );
        self.waiting = None;
        false
    }

    /// The reach record `seq` renders at: in the catch-up, the narrower of
    /// the device's then and now (module doc); after it, the reach now.
    fn reach_at(&self, seq: i64) -> AdminThreads {
        if seq <= self.caught_up_at {
            self.played.narrower(self.admin)
        } else {
            self.admin
        }
    }

    /// A level record `r` (`moved`), met in the catch-up (module doc): when
    /// it moved the device's reach, one `resync`, carrying `r`'s cursor, so
    /// a client that reconnects after reloading does not read it again. A
    /// move that kept the reach (`read_only` and `full`) changes nothing
    /// the device was shown: `hello` says the level now.
    fn flipped_while_away(&mut self, r: &table::Record, moved: Moved) {
        self.cursor = r.seq;
        self.cursor_tag = Some(r.tag.clone());
        self.played_levels = moved.applied(self.played_levels);
        let reach = self.played_levels.reach();
        if reach == self.played {
            return;
        }
        self.played = reach;
        self.sent = r.seq;
        self.out.push_back(
            resync(
                "what this device may see changed since the cursor it resumed from; reload \
                 what you show"
                    .into(),
            )
            .id(self.cursor_text(r.seq, Some(r.tag.clone()))),
        );
        self.resynced();
    }

    /// The levels are `levels` from here (a prune passed their records): a
    /// fresh `state` when they moved. The `resync` already said to reload.
    fn reach_is(&mut self, levels: table::Levels) {
        let now = levels.reach();
        self.played = now;
        self.played_levels = levels;
        if self.admin != now || self.levels != levels {
            self.admin = now;
            self.levels = levels;
            self.refresh_live();
        }
    }

    fn on_live(&mut self, got: Result<Live, broadcast::error::RecvError>) {
        let reason = match got {
            Ok(live) if live.is_devices_refresh() => {
                if self.device() {
                    self.refresh_live();
                }
                return;
            }
            Ok(live) => {
                if self.admin.sees(live.level) {
                    self.pending.push_back(Pending::Event(live));
                    // The table first: what a live event is about has been
                    // committed before it was published.
                    self.dirty = true;
                }
                return;
            }
            // A device is told "some": the count takes in events it would
            // never have received, and would say how busy what it may not
            // see is (review W4-14).
            Err(broadcast::error::RecvError::Lagged(missed)) => format!(
                "{} not delivered: this client read slower than they happened, past the \
                 feed's live buffer (Settings → Chat → Change feed → Live buffer: {} events). \
                 This is the live state now; stored events are not affected",
                if self.device() {
                    "some live events were".to_string()
                } else {
                    format!("{missed} live event(s) were")
                },
                self.state.chat_feed.live.capacity()
            ),
            Err(broadcast::error::RecvError::Closed) => format!(
                "the feed's live buffer was resized (Settings → Chat → Change feed → Live buffer: \
                 {} events). This is the live state now",
                self.state.chat_feed.live.capacity()
            ),
        };
        // Start over on the live side: a fresh subscription and the state
        // as of it, so what follows agrees with the state.
        let (rx, now) = self.state.chat_feed.live.subscribe(self.admin);
        self.live = rx;
        self.pending.clear();
        self.dirty = true;
        self.queue_state(now, reason);
    }

    /// A fresh `state` for this reader, after the table is read up to date.
    /// Neutral (review W6-4): a device is never told why, as the close and
    /// `current` are not (W4-18, W5-3).
    fn refresh_live(&mut self) {
        let now = self.state.chat_feed.live.now(self.admin);
        self.queue_state(now, "this is the live state now".to_string());
        self.dirty = true;
    }

    /// `now` as a `state` for the reach it was read at, the stream's now.
    fn queue_state(&mut self, now: Now, reason: String) {
        let level = self.levels.may_do();
        self.said = level;
        let data = now.state(level, Some(reason));
        self.pending.push_back(Pending::State {
            at: self.admin,
            level,
            data,
        });
    }

    /// Its key's admin-tools level, or the gateway's, moved (record `r`,
    /// module doc): when that moves the stream's reach, the threads and
    /// folders that come or go with it are said, the last of them carrying
    /// `r`'s cursor (a client that reconnects before it reads them all hears
    /// them again). A fresh `state` with the level follows any move of the
    /// device's own level, and a move of the gateway's that moved what the
    /// device may do. A read that fails is said as a `resync`.
    async fn reach_moved(
        &mut self,
        r: &table::Record,
        moved: Moved,
        purge: &super::super::chat_folders::retention::PurgeDays,
    ) {
        let (before, levels_before) = (self.admin, self.levels);
        self.levels = moved.applied(self.levels);
        self.admin = self.levels.reach();
        self.cursor = r.seq;
        self.cursor_tag = Some(r.tag.clone());
        if before == self.admin {
            let said_moves = match moved {
                Moved::Device(_) => levels_before.device != self.levels.device,
                Moved::Gateway(_) => levels_before.may_do() != self.levels.may_do(),
            };
            if said_moves {
                self.refresh_live();
            }
            return;
        }
        let frames = match super::render::reach_moved(&self.state, before, self.admin, purge).await
        {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("chat feed: the switch at {} could not be read: {e}", r.seq);
                self.out.push_back(resync(format!(
                    "what this device reaches changed and could not be read ({e}); reload what \
                     you show"
                )));
                self.resynced();
                self.refresh_live();
                return;
            }
        };
        let n = frames.len();
        for (i, (event, data)) in frames.into_iter().enumerate() {
            let mut frame = SseEvent::default().event(event).data(data.to_string());
            if i + 1 == n {
                frame = frame.id(self.cursor_text(r.seq, Some(r.tag.clone())));
                self.sent = r.seq;
            }
            self.out.push_back(frame);
        }
        self.refresh_live();
    }

    /// After a `resync` frame: the profile list again, as `profile.created`
    /// frames (personality-profiles design §3.2).
    fn resynced(&mut self) {
        self.out.extend(super::render::profile_list(&self.state));
    }

    /// A pending live frame goes out, when it still may (module doc).
    fn push_live(&mut self, p: Pending) {
        match p {
            Pending::Event(live) if self.admin.sees(live.level) => self.out.push_back(
                SseEvent::default()
                    .event(live.event)
                    .data(live.data.to_string()),
            ),
            Pending::State { at, level, data } if at == self.admin && level == self.said => {
                self.out.push_back(frame(event::STATE, &data))
            }
            // Out of the reach since it was received, or computed for
            // another reach or level: the move queued a fresh `state`.
            Pending::Event(_) | Pending::State { .. } => {}
        }
    }
}

/// A live-only frame (no `id:`).
pub(super) fn frame<T: serde::Serialize>(event: &'static str, data: &T) -> SseEvent {
    SseEvent::default()
        .event(event)
        .data(serde_json::to_string(data).unwrap_or_else(|_| "null".into()))
}

/// Where a stream resumed with `cursor` (`None`: none given) starts, given
/// this database's `epoch` and `bounds` (§2.4) and the record the table
/// holds at the cursor's number (`named`): the cursor itself when the table
/// still holds every record after it and that record is the one the cursor
/// names, else the newest record with a `resync` whose reason names the
/// cause — and the setting, for one the retention pruned past.
///
/// **The check** (review W5-4). A database restored from an older copy —
/// the file alone, or the whole data directory with it — keeps its epoch,
/// and once it has written past a client's cursor the numbers no longer
/// say anything is wrong. The cursor's tag does: the record at its number
/// is another one. A cursor at a number the table no longer holds (the
/// first kept is `pruned_through + 1`), without a tag, or with a tag of
/// another form than this build draws is checked by its numbers alone.
pub(super) fn start(
    cursor: Option<&lmgw_api_types::chat_feed::Cursor>,
    epoch: &str,
    bounds: table::Bounds,
    named: Option<&table::Record>,
    retention_days: i64,
) -> Start {
    let now = |reason: String| Start {
        cursor: bounds.head,
        resync: Some(reason),
    };
    let Some(c) = cursor else {
        return Start {
            cursor: bounds.head,
            resync: None,
        };
    };
    if c.epoch != epoch {
        return now(
            "the cursor is from another database (this gateway's data was replaced or reset \
             since it was issued); reload what you show"
                .into(),
        );
    }
    if c.seq > bounds.head {
        return now(format!(
            "the cursor ({}) is newer than the newest event ({}): this gateway's data was \
             restored from an older copy since it was issued; reload what you show",
            c.seq, bounds.head
        ));
    }
    if c.seq < bounds.pruned_through {
        return now(pruned_reason(retention_days, bounds.pruned_through));
    }
    // A tag of another form (the 8-hex-digit checks some earlier builds
    // issued, review F-10) says nothing about the record: such a cursor is
    // checked by its numbers, as an untagged one is.
    let tag = c.tag.as_deref().filter(|t| is_drawn_tag(t));
    if let (Some(tag), Some(r)) = (tag, named.filter(|r| r.seq == c.seq)) {
        if tag != r.tag {
            return now(format!(
                "the event the cursor names ({}) is not the one this gateway holds at that \
                 number: its data was restored from an older copy since the cursor was issued; \
                 reload what you show",
                c.seq
            ));
        }
    }
    Start {
        cursor: c.seq,
        resync: None,
    }
}

/// Whether `tag` has the form of the check this build draws for a record:
/// 16 hex digits (`lower(hex(randomblob(8)))`, migration 0065).
fn is_drawn_tag(tag: &str) -> bool {
    tag.len() == 16 && tag.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Why a cursor behind what retention pruned cannot be honoured, naming the
/// setting as the page labels it (review W4-12) — and what was pruned, when
/// the setting now keeps everything (review W4-24).
fn pruned_reason(retention_days: i64, pruned_through: i64) -> String {
    let setting = "Settings → Chat → Change feed → Keep changes for";
    if retention_days > 0 {
        format!(
            "the cursor is older than the feed keeps ({setting}: {retention_days} days); \
             reload what you show"
        )
    } else {
        format!(
            "the cursor is older than what the feed still holds: its records up to \
             {pruned_through} were pruned under an earlier {setting} (now 0, every record \
             kept); reload what you show"
        )
    }
}

/// The `resync` frame.
pub(super) fn resync(reason: String) -> SseEvent {
    frame(event::RESYNC, &Resync { reason })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::chat_feed::Cursor;

    fn c(epoch: &str, seq: i64) -> Cursor {
        Cursor {
            epoch: epoch.into(),
            seq,
            tag: None,
        }
    }

    #[test]
    fn a_cursor_is_honoured_only_while_every_record_after_it_is_kept() {
        let b = table::Bounds {
            pruned_through: 10,
            head: 20,
        };
        let s = start(None, "e", b, None, 7);
        assert_eq!((s.cursor, s.resync.is_none()), (20, true), "no cursor: now");
        for seq in [10, 15, 20] {
            let s = start(Some(&c("e", seq)), "e", b, None, 7);
            assert_eq!((s.cursor, s.resync), (seq, None));
        }
        let old = start(Some(&c("e", 9)), "e", b, None, 7);
        assert_eq!(old.cursor, 20);
        assert!(old
            .resync
            .unwrap()
            .contains("Change feed → Keep changes for: 7 days"));
        let zero = start(Some(&c("e", 9)), "e", b, None, 0);
        let zero = zero.resync.unwrap();
        assert!(
            zero.contains("up to 10 were pruned") && zero.contains("now 0"),
            "{zero}"
        );
        let ahead = start(Some(&c("e", 21)), "e", b, None, 7);
        assert!(ahead.resync.unwrap().contains("newer than the newest"));
        let foreign = start(Some(&c("f", 15)), "e", b, None, 7);
        assert!(foreign.resync.unwrap().contains("another database"));
    }

    /// A stream reading `state`'s table from `cursor`, for `admin`.
    fn stream_at(state: &SharedState, cursor: i64, page_size: u32) -> FeedStream {
        FeedStream {
            state: state.clone(),
            admin: AdminThreads::Shown,
            levels: table::Levels {
                device: DeviceAdmin::Off,
                gateway: SelfAdmin::ReadOnly,
            },
            said: SelfAdmin::Off,
            played: AdminThreads::Shown,
            played_levels: table::Levels {
                device: DeviceAdmin::Off,
                gateway: SelfAdmin::ReadOnly,
            },
            caught_up_at: 0,
            key_id: None,
            epoch: "e".into(),
            cursor,
            cursor_tag: None,
            sent: cursor,
            page_size,
            stored: state.chat_feed.stored.subscribe(),
            live: state.chat_feed.live.subscribe(AdminThreads::Shown).0,
            keepalive: FeedStream::keepalive(3600),
            conn: None,
            out: VecDeque::new(),
            pending: VecDeque::new(),
            dirty: true,
            ended: false,
            stalled: None,
            waiting: None,
        }
    }

    /// Review W4-4: retention that prunes past a stream still catching up
    /// is said, with a `resync`, and the stream goes on from the newest.
    #[tokio::test]
    async fn a_prune_past_a_stream_mid_catch_up_is_a_resync() {
        let state = crate::state::AppState::init_for_tests().await.unwrap();
        for _ in 0..4 {
            crate::store::create_chat_thread(&state.db, "m", "chat")
                .await
                .unwrap();
        }
        let mut s = stream_at(&state, 0, 1);
        s.read_page().await;
        assert_eq!((s.cursor, s.out.len()), (1, 1), "one page of one");
        // The rest is pruned while the stream is behind.
        sqlx::query("UPDATE chat_feed SET at = datetime('now', '-30 days')")
            .execute(&state.db)
            .await
            .unwrap();
        assert_eq!(table::prune(&state.db, 7).await.unwrap(), 4);
        s.out.clear();
        s.read_page().await;
        // The resync, then the profile list (personality-profiles §3.2).
        let said = 1 + state.snapshot().chat_profiles.len();
        assert_eq!(s.out.len(), said);
        assert_eq!(s.cursor, 4, "on from the newest");
        assert!(s.dirty);
        s.read_page().await;
        assert!(s.out.len() == said && !s.dirty, "nothing more to read");
    }

    /// Review G-9: a prune past a `gateway.reach` record its stream was
    /// behind on: the `resync`, then the levels read again with the head
    /// (`reach_is`) — the gateway's `off` takes the toolset's reach from a
    /// device at `read_only`, and a fresh `state` says `off`.
    #[tokio::test]
    async fn a_prune_past_the_gateway_s_level_reads_it_again() {
        let state = crate::state::AppState::init_for_tests().await.unwrap();
        let mut settings = state.snapshot().settings.clone();
        settings.self_admin = SelfAdmin::ReadOnly;
        crate::store::save_settings(&state.db, &settings)
            .await
            .unwrap();
        let id = crate::store::insert_device_key(
            &state.db,
            "device:tablet",
            "hash",
            &crate::config::KeyPolicy::default(),
            (None, DeviceAdmin::ReadOnly),
            "",
        )
        .await
        .unwrap();
        let head = table::bounds(&state.db).await.unwrap().head;
        let levels = table::Levels {
            device: DeviceAdmin::ReadOnly,
            gateway: SelfAdmin::ReadOnly,
        };
        let mut s = FeedStream {
            admin: levels.reach(),
            levels,
            said: SelfAdmin::ReadOnly,
            played: levels.reach(),
            played_levels: levels,
            key_id: Some(id),
            ..stream_at(&state, head, 1)
        };
        crate::store::create_chat_thread(&state.db, "m", "chat")
            .await
            .unwrap();
        settings.self_admin = SelfAdmin::Off;
        crate::store::save_settings(&state.db, &settings)
            .await
            .unwrap();
        sqlx::query("UPDATE chat_feed SET at = datetime('now', '-30 days')")
            .execute(&state.db)
            .await
            .unwrap();
        assert!(table::prune(&state.db, 7).await.unwrap() >= 2);
        s.read_page().await;
        // The resync, then the profile list (personality-profiles §3.2).
        assert_eq!(
            s.out.len(),
            1 + state.snapshot().chat_profiles.len(),
            "the resync"
        );
        assert_eq!(s.admin, AdminThreads::Hidden, "the toolset's reach is gone");
        assert_eq!(s.levels.gateway, SelfAdmin::Off);
        assert!(
            s.pending.iter().any(|p| matches!(
                p,
                Pending::State {
                    level: SelfAdmin::Off,
                    ..
                }
            )),
            "a fresh state says off"
        );
    }

    /// Review W5-4: a cursor names its record by a check; a database
    /// restored from an older copy that wrote other records at the same
    /// numbers since is another database for it, whatever the numbers say.
    #[test]
    fn a_cursor_whose_record_differs_is_answered_with_a_resync() {
        let b = table::Bounds {
            pruned_through: 0,
            head: 20,
        };
        let record = |tag: &str| table::Record {
            seq: 15,
            at: "2026-10-07 08:00:00".into(),
            kind: "thread.created".into(),
            thread_id: Some(4),
            folder_id: None,
            message_ids: None,
            by: None,
            admin: 0,
            detail: None,
            tag: tag.into(),
        };
        let held = record("9f00c1d2aa01bb02");
        let tagged = Cursor {
            tag: Some(held.tag.clone()),
            ..c("e", 15)
        };
        let same = start(Some(&tagged), "e", b, Some(&held), 7);
        assert_eq!((same.cursor, same.resync), (15, None));
        // The same number, written again after a restore: the same facts,
        // even in the same second, and another tag drawn for it.
        let rewritten = record("0c0ffee000000001");
        let other = start(Some(&tagged), "e", b, Some(&rewritten), 7);
        assert_eq!(other.cursor, 20);
        assert!(
            other
                .resync
                .unwrap()
                .contains("restored from an older copy"),
            "the record the cursor names is not this one"
        );
        // Untagged, or at a number the table no longer holds: the numbers.
        let untagged = start(Some(&c("e", 15)), "e", b, Some(&rewritten), 7);
        assert_eq!(untagged.resync, None);
        // A check of an earlier form (8 hex digits) is no check of this
        // record (review F-10): no false "restored" resync.
        let earlier = Cursor {
            tag: Some("9f00c1d2".into()),
            ..c("e", 15)
        };
        let earlier = start(Some(&earlier), "e", b, Some(&held), 7);
        assert_eq!((earlier.cursor, earlier.resync), (15, None));
        let unread = start(Some(&tagged), "e", b, None, 7);
        assert_eq!(unread.resync, None);
    }

    /// Review W5-16: a record that keeps failing to render is passed, with
    /// a `resync` that names it, after `RENDER_ATTEMPTS` reads: never a
    /// stall for good, never a silent skip.
    #[tokio::test]
    async fn a_record_that_never_renders_is_passed_with_a_resync() {
        let state = crate::state::AppState::init_for_tests().await.unwrap();
        crate::store::create_chat_thread(&state.db, "m", "chat")
            .await
            .unwrap();
        // The thread can no longer be read: every render of its record fails.
        sqlx::query("ALTER TABLE chat_threads RENAME TO chat_threads_away")
            .execute(&state.db)
            .await
            .unwrap();
        let mut s = stream_at(&state, 0, 10);
        for _ in 1..RENDER_ATTEMPTS {
            s.read_page().await;
            assert!(s.out.is_empty() && s.cursor == 0 && !s.dirty, "it waits");
            s.dirty = true;
        }
        s.read_page().await;
        // The resync, then the profile list (personality-profiles §3.2).
        let profiles = state.snapshot().chat_profiles.len();
        assert!(profiles > 0, "the built-in is seeded");
        assert_eq!(
            (s.cursor, s.out.len()),
            (1, 1 + profiles),
            "passed, with its resync"
        );
        assert!(s.cursor_tag.is_some());
        sqlx::query("ALTER TABLE chat_threads_away RENAME TO chat_threads")
            .execute(&state.db)
            .await
            .unwrap();
    }

    /// Review W4-5: a read that fails while a record is rendered is an
    /// error — the stream waits at the record — never a tombstone.
    #[tokio::test]
    async fn a_failed_read_is_no_tombstone() {
        let state = crate::state::AppState::init_for_tests().await.unwrap();
        crate::store::create_chat_thread(&state.db, "m", "chat")
            .await
            .unwrap();
        let record = table::page(&state.db, 0, 10).await.unwrap().remove(0);
        state.db.close().await;
        let purge = super::super::super::chat_folders::retention::PurgeDays::fixed(30, &[]);
        let rendered =
            super::super::render::render(&state, &record, (AdminThreads::Shown, None), &purge)
                .await;
        assert!(rendered.is_err(), "{rendered:?}");
    }
}
