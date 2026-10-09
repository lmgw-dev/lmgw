//! A stored record as a client receives it (client-apps design §2.2, §2.3):
//! rendered at delivery from the thread or folder as it is now, or a
//! tombstone for one that is gone — and not at all for a principal that may
//! not see it (L3).

use lmgw_api_types::chat_feed::{
    FeedProfile, FolderChanged, FolderCurrent, FolderGone, ThreadChanged, ThreadGone,
};
use serde_json::{json, Value};

use super::super::chat_folders::retention::PurgeDays;
use super::super::chat_wire::{folder, last_messages, thread_row, wire};

use crate::state::AppState;
use crate::store::feed::{kind, Record};
use crate::store::{self, AdminThreads};

/// `(event, data)` of `r` for a reader that reaches as far as `admin` into
/// the threads and folders that drive the self-admin plane (the owner all of
/// them; a device allowed lmgw's admin tools the toolset's; any other device
/// none; L3); `None` when the reader may not see it. A record type this
/// build does not render (written by a newer one) is skipped, with a log
/// line. A device's switch record (`device.reach`) is its stream's own
/// business (`stream`), and renders as nothing here.
///
/// **A read that fails is an error, never a tombstone** (review W4-5): the
/// stream stops its page before the record, keeps its cursor there, and
/// reads it again at its next wake or keep-alive tick. Only a thread or
/// folder that is really gone renders as `deleted`.
///
/// **A device and the self-admin toolset** (review W3-1). Attaching it to a
/// thread, or to a folder's defaults, takes the thread or folder out of the
/// reach of a device not allowed lmgw's admin tools; removing it brings it
/// back; a device's delete hides a folder from every device, and the owner
/// shows it again (review W6-1, F-7). The record of that write says so
/// (`Record::admin_was`), and a device that saw it before and not now, or
/// the other way round, receives it as the thread's or folder's removal
/// (`*.deleted`) or arrival (`*.created`).
///
/// `reader`: the device key the stream reads for (`None`: the owner) — a
/// `device.revoked` record also reaches the hosts it names
/// ([`super::devices`]).
pub(super) async fn render(
    state: &AppState,
    r: &Record,
    (admin, reader): (AdminThreads, Option<i64>),
    purge: &PurgeDays,
) -> Result<Option<(String, Value)>, crate::error::GatewayError> {
    let device = admin.is_device();
    let by = r.by.clone();
    let mut event = r.kind.clone();
    if r.kind == kind::DEVICE_REACH || r.kind == kind::GATEWAY_REACH {
        return Ok(None);
    }
    if r.kind == kind::DEVICE_REVOKED {
        return Ok(super::devices::render(r, admin, reader).map(|data| (event, data)));
    }
    if device {
        let seen_before = r.admin_was().map(|was| admin.sees(was));
        match (admin.sees(r.admin), seen_before) {
            // Out of its reach with this write: it is gone, for the device.
            (false, Some(true)) => {
                return Ok(match r.kind.as_str() {
                    kind::FOLDER_UPDATED => r
                        .folder_id
                        .map(|id| (kind::FOLDER_DELETED.to_string(), gone_folder(id, by))),
                    _ => r
                        .thread_id
                        .map(|id| (kind::THREAD_DELETED.to_string(), gone_thread(id, by))),
                });
            }
            (false, _) => return Ok(None),
            // Back in its reach with this write: new, for the device.
            (true, Some(false)) => {
                event = match r.kind.as_str() {
                    kind::FOLDER_UPDATED => kind::FOLDER_CREATED.to_string(),
                    _ => kind::THREAD_CREATED.to_string(),
                };
            }
            (true, _) => {}
        }
    }
    let data = match r.kind.as_str() {
        kind::THREAD_CREATED | kind::THREAD_UPDATED => {
            let Some(id) = r.thread_id else {
                return Ok(None);
            };
            match store::get_chat_thread(&state.db, id).await? {
                Some(t) => {
                    // As it is now: one a later write took out of a
                    // device's reach is not shown (that write's own record
                    // says it went).
                    if !admin.sees(t.reach_level()) {
                        return Ok(None);
                    }
                    // In a folder the device cannot see: in none, for it
                    // (review W4-7).
                    let mut t = t;
                    if let (true, Some(f)) = (device, t.folder_id) {
                        if store::get_chat_folder(&state.db, f)
                            .await?
                            .is_some_and(|f| !admin.sees(f.reach_level()))
                        {
                            t.folder_id = None;
                        }
                    }
                    let last = last_messages(state, &[&t]).await.get(&t.id).copied();
                    wire(&ThreadChanged {
                        thread: thread_row(&t, purge, last),
                        by,
                    })
                }
                None => gone_thread(id, by),
            }
        }
        kind::THREAD_DELETED => match r.thread_id {
            Some(id) => gone_thread(id, by),
            None => return Ok(None),
        },
        kind::FOLDER_CREATED | kind::FOLDER_UPDATED => {
            let Some(id) = r.folder_id else {
                return Ok(None);
            };
            match store::get_chat_folder_listed(&state.db, id, admin).await? {
                Some(f) => {
                    if !admin.sees(f.folder.reach_level()) {
                        return Ok(None);
                    }
                    wire(&FolderChanged {
                        folder: folder(&f),
                        by,
                    })
                }
                None => gone_folder(id, by),
            }
        }
        kind::FOLDER_DELETED => match r.folder_id {
            Some(id) => gone_folder(id, by),
            None => return Ok(None),
        },
        kind::FOLDER_CURRENT => {
            let Some(folder_id) = r.folder_id else {
                return Ok(None);
            };
            let detail: Value = r
                .detail
                .as_deref()
                .and_then(|d| serde_json::from_str(d).ok())
                .unwrap_or(Value::Null);
            let mut previous = detail.get("previous_thread_id").and_then(Value::as_i64);
            let mut reason = detail
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if device {
                // As things are now (review W4-6): a folder or a thread a
                // later write took out of a device's reach is not shown —
                // that write's own record says it went. A previous thread
                // out of its reach is not named.
                let folder = store::get_chat_folder(&state.db, folder_id).await?;
                if folder.is_some_and(|f| !admin.sees(f.reach_level())) {
                    return Ok(None);
                }
                if let Some(id) = r.thread_id {
                    if hidden_thread(state, id, admin).await? {
                        return Ok(None);
                    }
                }
                if let Some(id) = previous {
                    if !admin.sees(r.previous_level()) || hidden_thread(state, id, admin).await? {
                        previous = None;
                        // Never why it went (review W5-3): for the device
                        // the folder had no current thread, as its folder
                        // JSON says, and a move to none is no news. Every
                        // reason is `first` then (review W6-15): `idle` or
                        // `requested` with no previous thread would say
                        // that one was there.
                        if r.thread_id.is_none() {
                            return Ok(None);
                        }
                        reason = "first".into();
                    }
                }
            }
            json!(FolderCurrent {
                folder_id,
                thread_id: r.thread_id,
                previous_thread_id: previous,
                reason,
                by,
            })
        }
        // A gated call and its decision (client-apps design §6.5), from the
        // reply's pending state now.
        kind::APPROVAL_REQUESTED | kind::APPROVAL_DECIDED => {
            match super::approvals::render(state, r, admin).await? {
                Some(data) => data,
                None => return Ok(None),
            }
        }
        // An MCP task a turn started, and its result entering the thread
        // (MCP Tasks design §4.1), from the facts the record keeps.
        kind::TASK_STARTED | kind::TASK_DONE => {
            match super::tasks::render(state, r, admin).await? {
                Some(data) => data,
                None => return Ok(None),
            }
        }
        // A profile is every reader's (personality-profiles design §3.2):
        // named as its write left it.
        kind::PROFILE_CREATED | kind::PROFILE_UPDATED | kind::PROFILE_DELETED => {
            let Some((id, name)) = r.profile() else {
                return Ok(None);
            };
            json!(FeedProfile { id, name, by })
        }
        other => {
            tracing::debug!(
                "chat feed: record {} has a type this build does not render: {other}",
                r.seq
            );
            return Ok(None);
        }
    };
    Ok(Some((event, data)))
}

/// Whether thread `id` is out of the reach of a reader that reaches as far
/// as `admin` now, for a device's rendering of a record that names it.
async fn hidden_thread(
    state: &AppState,
    id: i64,
    admin: AdminThreads,
) -> Result<bool, crate::error::GatewayError> {
    Ok(store::get_chat_thread(&state.db, id)
        .await?
        .is_some_and(|t| !admin.sees(t.reach_level())))
}

/// The whole profile list as `profile.created` frames, with no `id:` and
/// no author: what follows every `resync` (personality-profiles design
/// §3.2), so a client that keeps the names has them again without a
/// request. From the published snapshot, which every profile write
/// publishes before it wakes the feed.
pub(super) fn profile_list(state: &AppState) -> Vec<axum::response::sse::Event> {
    state
        .snapshot()
        .chat_profiles
        .iter()
        .map(|p| {
            super::stream::frame(
                lmgw_api_types::chat_feed::event::PROFILE_CREATED,
                &FeedProfile {
                    id: p.id,
                    name: p.name.clone(),
                    by: None,
                },
            )
        })
        .collect()
}

fn gone_thread(thread_id: i64, by: Option<String>) -> Value {
    json!(ThreadGone {
        thread_id,
        deleted: true,
        by,
    })
}

fn gone_folder(folder_id: i64, by: Option<String>) -> Value {
    json!(FolderGone {
        folder_id,
        deleted: true,
        by,
    })
}

/// What a device's stream says when its reach moves from `before` to
/// `after` — its admin-tools switch turned on or off (`stream`'s module
/// doc) — as `(event, data)` in order, each rendered for `after` as the
/// thread or folder is now, by the owner who changed it.
///
/// - **On** (`after` sees more): the folders only `after` sees as
///   `folder.created`; the folders both see whose counts or current thread
///   move with it as `folder.updated`; the threads only `after` sees as
///   `thread.created`; the threads both see that sit in a folder only
///   `after` sees as `thread.updated` (in it now).
/// - **Off** (`after` sees less): a delete's order — the threads only
///   `before` saw as `thread.deleted`; the threads both see in a folder only
///   `before` saw as `thread.updated` (in no folder now); the folders both
///   see whose counts or current thread move as `folder.updated`; the
///   folders only `before` saw as `folder.deleted`.
pub(super) async fn reach_moved(
    state: &AppState,
    before: AdminThreads,
    after: AdminThreads,
    purge: &PurgeDays,
) -> Result<Vec<(String, Value)>, crate::error::GatewayError> {
    use std::collections::{HashMap, HashSet};
    let gained = after.reach() > before.reach();
    let (wide, narrow) = if gained {
        (after, before)
    } else {
        (before, after)
    };
    let by = Some(crate::store::feed::BY_OWNER.to_string());
    // Everything the wider reach sees, levels and all.
    let threads = store::list_chat_threads_as(&state.db, store::ThreadListMode::All, wide).await?;
    let wide_folders = store::list_chat_folders(&state.db, wide).await?;
    let gap = |level: u8| wide.sees(level) && !narrow.sees(level);
    let moved_folders: HashSet<i64> = wide_folders
        .iter()
        .filter(|f| gap(f.folder.reach_level()))
        .map(|f| f.folder.id)
        .collect();
    let level_of: HashMap<i64, u8> = threads.iter().map(|t| (t.id, t.reach_level())).collect();
    let gap_threads: Vec<&store::ChatThread> =
        threads.iter().filter(|t| gap(t.reach_level())).collect();
    let in_moved: Vec<&store::ChatThread> = threads
        .iter()
        .filter(|t| {
            !gap(t.reach_level()) && t.folder_id.is_some_and(|f| moved_folders.contains(&f))
        })
        .collect();
    // Folders both see whose counts or current thread move with the reach.
    let touched: HashSet<i64> = wide_folders
        .iter()
        .filter(|f| !moved_folders.contains(&f.folder.id))
        .filter(|f| {
            gap_threads.iter().any(|t| t.folder_id == Some(f.folder.id))
                || f.folder
                    .current_thread_id()
                    .and_then(|c| level_of.get(&c))
                    .is_some_and(|l| gap(*l))
        })
        .map(|f| f.folder.id)
        .collect();
    // Rendered for `after`: counts, current thread and folders masked.
    let after_folders: HashMap<i64, store::ChatFolderListed> =
        store::list_chat_folders(&state.db, after)
            .await?
            .into_iter()
            .map(|f| (f.folder.id, f))
            .collect();
    let hidden_after = store::self_admin_folder_ids(&state.db, after).await?;
    let all: Vec<&store::ChatThread> = gap_threads.iter().chain(in_moved.iter()).copied().collect();
    let last = last_messages(state, &all).await;
    let row = |t: &store::ChatThread| {
        let mut t = t.clone();
        if t.folder_id.is_some_and(|f| hidden_after.contains(&f)) {
            t.folder_id = None;
        }
        let last_message_at = last.get(&t.id).copied();
        wire(&ThreadChanged {
            thread: thread_row(&t, purge, last_message_at),
            by: by.clone(),
        })
    };
    let folder_now = |id: i64| {
        after_folders.get(&id).map(|f| {
            wire(&FolderChanged {
                folder: folder(f),
                by: by.clone(),
            })
        })
    };
    let mut out = Vec::new();
    let updated_folders = |out: &mut Vec<(String, Value)>| {
        for id in &touched {
            if let Some(data) = folder_now(*id) {
                out.push((kind::FOLDER_UPDATED.to_string(), data));
            }
        }
    };
    if gained {
        for f in wide_folders
            .iter()
            .filter(|f| moved_folders.contains(&f.folder.id))
        {
            if let Some(data) = folder_now(f.folder.id) {
                out.push((kind::FOLDER_CREATED.to_string(), data));
            }
        }
        updated_folders(&mut out);
        for t in &gap_threads {
            out.push((kind::THREAD_CREATED.to_string(), row(t)));
        }
        for t in &in_moved {
            out.push((kind::THREAD_UPDATED.to_string(), row(t)));
        }
    } else {
        for t in &gap_threads {
            out.push((
                kind::THREAD_DELETED.to_string(),
                gone_thread(t.id, by.clone()),
            ));
        }
        for t in &in_moved {
            out.push((kind::THREAD_UPDATED.to_string(), row(t)));
        }
        updated_folders(&mut out);
        for f in wide_folders
            .iter()
            .filter(|f| moved_folders.contains(&f.folder.id))
        {
            out.push((
                kind::FOLDER_DELETED.to_string(),
                gone_folder(f.folder.id, by.clone()),
            ));
        }
    }
    Ok(out)
}
