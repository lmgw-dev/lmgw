//! `POST /chat/api/threads/{id}/media/{call_id}/cancel` (chat-media-tools
//! design §6.5, `Cap::Chat`): raises the stop of a running image or audio
//! call of the thread, which ends it the way the turn's Stop does — a
//! running image job cancelled by its guard (`proxy::image_call`), its
//! rows written `canceled` — and the turn goes on, its model told the
//! picture was not made.
//!
//! The answer says what the cancel does to the job, as the call's last
//! progress frame said it (`native`, `restart`, `abandon`), or nothing when
//! no job runs yet. A thread the caller does not reach is the `404
//! not_found` of a thread that is not there (L3); a call that is not
//! running, `404 not_running`.
//!
//! A thread that goes away — deleted, kept as a stored chat, purged —
//! stops every media call still running in it ([`MediaCalls::thread_gone`]),
//! so none stores a file into a thread that is no longer there.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use lmgw_api_types::chat_media::{CancelKind, MediaCancelled};

use super::super::chat::err_json;
use super::super::chat_caller::Caller;
use super::super::chat_extract::ChatPath;
use super::super::chat_repo::ChatRepo;
use crate::proxy::StopHandle;
use crate::state::SharedState;

/// The running media calls of every thread, by `(thread id, call id)`.
#[derive(Default)]
pub(crate) struct MediaCalls(Mutex<HashMap<(i64, String), Arc<Running>>>);

/// One running call.
struct Running {
    stop: Arc<StopHandle>,
    /// What a cancel would do to its job now; `None` before a job runs.
    cancel: Mutex<Option<CancelKind>>,
    /// Its thread went away while it ran ([`MediaCalls::thread_gone`]).
    gone: AtomicBool,
}

/// A call's place among the running ones, given up when dropped.
pub(crate) struct Registered {
    state: SharedState,
    key: (i64, String),
    running: Arc<Running>,
}

impl MediaCalls {
    /// Call `call_id` of thread `thread_id` runs, stopped by `stop`.
    pub(crate) fn register(
        &self,
        state: &SharedState,
        (thread_id, call_id): (i64, &str),
        stop: Arc<StopHandle>,
    ) -> Registered {
        let key = (thread_id, call_id.to_string());
        let running = Arc::new(Running {
            stop,
            cancel: Mutex::new(None),
            gone: AtomicBool::new(false),
        });
        self.lock().insert(key.clone(), running.clone());
        Registered {
            state: state.clone(),
            key,
            running,
        }
    }

    /// Stop the call: what it does to its job, or `None` (outer) when no
    /// such call runs.
    fn cancel(&self, thread_id: i64, call_id: &str) -> Option<Option<CancelKind>> {
        let running = self
            .lock()
            .get(&(thread_id, call_id.to_string()))
            .cloned()?;
        let kind = *running.cancel.lock().unwrap_or_else(|e| e.into_inner());
        running.stop.stop();
        Some(kind)
    }

    /// Thread `thread_id` went away (module doc): stop every call running
    /// in it. How many were.
    pub(crate) fn thread_gone(&self, thread_id: i64) -> usize {
        let running: Vec<Arc<Running>> = self
            .lock()
            .iter()
            .filter(|((t, _), _)| *t == thread_id)
            .map(|(_, r)| r.clone())
            .collect();
        for r in &running {
            r.gone.store(true, Ordering::Relaxed);
            r.stop.stop();
        }
        running.len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(i64, String), Arc<Running>>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Registered {
    /// Raise its own stop (its caller's key was revoked).
    pub(crate) fn stop(&self) {
        self.running.stop.stop();
    }

    /// Whether its thread went away while it ran.
    pub(crate) fn thread_gone(&self) -> bool {
        self.running.gone.load(Ordering::Relaxed)
    }

    /// What a cancel would do now.
    pub(crate) fn set_cancel(&self, kind: Option<CancelKind>) {
        *self
            .running
            .cancel
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = kind;
    }
}

impl Drop for Registered {
    fn drop(&mut self) {
        let mut map = self.state.media_calls.lock();
        // Only its own entry: a model that reused a call id in a later
        // turn registered another.
        if map
            .get(&self.key)
            .is_some_and(|r| Arc::ptr_eq(r, &self.running))
        {
            map.remove(&self.key);
        }
    }
}

/// The route (module doc).
pub(in crate::web) async fn cancel(
    State(state): State<SharedState>,
    caller: Caller,
    ChatPath((id, call_id)): ChatPath<(i64, String)>,
) -> Response {
    match ChatRepo::of(id).thread_as(&state, &caller, id).await {
        Ok(Some(_)) => {}
        Ok(None) => return err_json(StatusCode::NOT_FOUND, "not_found", "thread not found"),
        Err(e) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
    match state.media_calls.cancel(id, &call_id) {
        Some(cancel) => Json(MediaCancelled { ok: true, cancel }).into_response(),
        None => err_json(
            StatusCode::NOT_FOUND,
            "not_running",
            format!("no image or audio call '{call_id}' is running in thread {id}"),
        ),
    }
}
