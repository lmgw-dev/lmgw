//! What the pick cache ([`super::derive::cached_pick`]) checks on every call
//! instead of deriving again (review R, finding 3; §12 entry 87): the files a
//! candidate's published capabilities are read from, with their size and
//! modification time.
//!
//! A pick is derived from the snapshot **and** from files — each candidate's
//! weights header (its embedded chat template), its configured projector's
//! header (image and audio input) and its chat template file — and those
//! change without the snapshot changing: an HF update rewriting a GGUF in
//! place, a file still being written, a mount that was not there yet. So the
//! cache keeps the stamp of every such file next to the pick, and a call
//! whose stamps differ derives again — the revalidation
//! [`crate::gguf::GgufSummaryCache`] does for the headers themselves. A pick
//! derived while any of those files could not be read is never cached, the
//! same rule as that cache's "errors are never cached": a transient problem
//! must not decide where requests go until the next config write.
//!
//! **Cost.** One `stat` per file per candidate per call — one to three per
//! candidate, usually one — run together on the blocking pool; nothing is
//! read or parsed. A ladder's higher rungs are not stamped: their headers
//! feed only the published context, which a pick does not carry. The alias
//! fallback's capabilities are not part of the pick the gate reads at all:
//! it judges the fallback at use, from the live catalog cache
//! ([`crate::gate::open::fallback_serves`]).

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::capabilities::configured_projector;
use crate::config::{CandidateAlias, Snapshot};
use crate::state::SharedState;

/// What a stamped file is read as — a GGUF header through the GGUF cache,
/// or plain text (a chat template file).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Gguf,
    Text,
}

/// One file a pick was derived from, and what `stat` said about it: its
/// size and modification time, `None` when it could not be stat'ed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stamp {
    path: PathBuf,
    kind: Kind,
    seen: Option<(u64, Option<SystemTime>)>,
}

/// The files the capabilities of `alias`'s candidates are read from: every
/// enabled local row's weights, configured projector and chat template file,
/// resolved against the chat models dir exactly as
/// `capabilities::exposed::local_derived` resolves them. A candidate that is
/// missing or disabled reads nothing — that state is the snapshot's.
fn files(snap: &Snapshot, alias: &CandidateAlias) -> Vec<(PathBuf, Kind)> {
    let dir = Path::new(&snap.settings.router.models_dir);
    let mut out = Vec::new();
    for id in &alias.candidates {
        let Some(m) = snap
            .local_models
            .iter()
            .find(|m| &m.model_id == id && m.enabled)
        else {
            continue;
        };
        out.push((dir.join(&m.gguf_path), Kind::Gguf));
        if let Some(projector) = configured_projector(m) {
            out.push((dir.join(projector), Kind::Gguf));
        }
        if let Some(template) = m
            .params
            .chat_template_file
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            out.push((dir.join(template), Kind::Text));
        }
    }
    out
}

/// The stamps of every file `alias`'s pick is derived from, right now.
pub(crate) async fn stamp(snap: &Snapshot, alias: &CandidateAlias) -> Vec<Stamp> {
    let files = files(snap, alias);
    if files.is_empty() {
        return Vec::new();
    }
    let unread = files
        .iter()
        .map(|(path, kind)| Stamp {
            path: path.clone(),
            kind: *kind,
            seen: None,
        })
        .collect::<Vec<_>>();
    tokio::task::spawn_blocking(move || {
        files
            .into_iter()
            .map(|(path, kind)| {
                let seen = std::fs::metadata(&path)
                    .ok()
                    .map(|m| (m.len(), m.modified().ok()));
                Stamp { path, kind, seen }
            })
            .collect()
    })
    .await
    // The blocking pool could not run it: nothing was stat'ed, so nothing
    // may be cached against it.
    .unwrap_or(unread)
}

/// Whether every stamped file could be read: stat'ed, and for a GGUF its
/// header parsed. Through the GGUF cache, which keeps good headers and never
/// errors, so for a file the derive just read this is a stat and a lookup.
pub(crate) async fn all_readable(state: &SharedState, stamps: &[Stamp]) -> bool {
    for s in stamps {
        if s.seen.is_none() {
            return false;
        }
        if s.kind == Kind::Gguf && state.gguf_cache.summarize_cached(&s.path).await.is_err() {
            return false;
        }
    }
    true
}
