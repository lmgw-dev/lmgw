//! `POST /chat/api/threads/{id}/settings`: a thread's settings patch, and
//! its checks as one step ([`apply_settings_patch`]) — what a folder patch
//! runs on the folder's current thread too (client-apps design L9), so the
//! current thread refuses exactly what the route refuses.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use super::super::chat_caller::Caller;
use super::super::chat_extract::{ChatJson, ChatPath};
use super::super::chat_repo::ChatRepo;
use super::super::chat_steer::{self, Change};
use super::super::chat_turn;
use super::super::{chat_knowledge, chat_reasoning, chat_sampling, chat_tool_write, chat_voice};
use super::err_json;
use crate::error::GatewayError;
use crate::state::SharedState;
use crate::store::{ChatThread, SeedWrite, ThreadMcp};

/// A **patch**: an absent field leaves that setting alone.
///
/// The page patches this endpoint from two places with different halves — the
/// header's model picker sends only `model_alias`, the settings drawer sends
/// everything but — so "absent" has to mean "unchanged" or each save blanks
/// what the other owns. The sampling fields are doubly wrapped because `null`
/// is meaningful for them: absent keeps the value, `null` clears it back to the
/// upstream's own default.
#[derive(Deserialize, Default, Clone)]
pub(crate) struct SettingsReq {
    model_alias: Option<String>,
    system_prompt: Option<String>,
    #[serde(default, deserialize_with = "present")]
    temperature: Option<Option<f64>>,
    #[serde(default, deserialize_with = "present")]
    max_tokens: Option<Option<i64>>,
    /// Registered MCP servers this thread attaches.
    mcp_tools: Option<Vec<ThreadMcp>>,
    /// The reasoning overrides ([`super::chat_reasoning`]), wrapped like the
    /// sampling fields: `null` clears one back to the route's default.
    #[serde(default, deserialize_with = "present")]
    reasoning_enabled: Option<Option<bool>>,
    #[serde(default, deserialize_with = "present")]
    reasoning_effort: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    reasoning_budget: Option<Option<i64>>,
    /// The sampling overrides ([`super::chat_sampling`]), wrapped the same
    /// way; `stop` is the whole list (`[]` clears it).
    #[serde(default, deserialize_with = "present")]
    top_p: Option<Option<f64>>,
    #[serde(default, deserialize_with = "present")]
    top_k: Option<Option<i64>>,
    #[serde(default, deserialize_with = "present")]
    min_p: Option<Option<f64>>,
    #[serde(default, deserialize_with = "present")]
    repeat_penalty: Option<Option<f64>>,
    #[serde(default, deserialize_with = "present")]
    presence_penalty: Option<Option<f64>>,
    #[serde(default, deserialize_with = "present")]
    frequency_penalty: Option<Option<f64>>,
    #[serde(default, deserialize_with = "present")]
    seed: Option<Option<i64>>,
    stop: Option<Vec<String>>,
    /// Knowledge bases ([`super::chat_knowledge`]): the whole selection
    /// (`[]` clears it), `"auto"` | `"tool"`, and the retrieval budget
    /// (`null` = the `chat_kb_budget_tokens` setting).
    kb_ids: Option<Vec<i64>>,
    kb_mode: Option<String>,
    #[serde(default, deserialize_with = "present")]
    kb_budget_tokens: Option<Option<i64>>,
    /// The voice overrides as a whole object (chat-voice design §2.2,
    /// [`super::chat_voice::apply_thread_voice`]); `null` clears them.
    #[serde(default, deserialize_with = "present")]
    voice: Option<Value>,
}

/// `Some(value)` for a field that was sent — including one sent as `null`,
/// which a bare `Option<Option<T>>` would flatten into "absent" and so make
/// clearing a sampling setting impossible.
pub(crate) fn present<'de, T, D>(de: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    T::deserialize(de).map(Some)
}

/// `POST /chat/api/threads/{id}/settings` — patch model + sampling settings,
/// the reasoning overrides, the thread's attached MCP servers, its
/// knowledge bases and its voice (the title is preserved; it auto-names on first send). Overrides that contradict each
/// other are a 400 `bad_request`, and nothing is written. Answers `{ok,
/// continue, voice, voice_resolved}`: the thread's `continue` re-judged under
/// the new settings (a model switch or reasoning toggle changes it), its
/// voice as stored and what that resolves to now. A device's `mcp_tools`
/// pass its tool scope first (client-apps design L5): `403
/// tool_label_out_of_scope` otherwise, nothing written.
pub async fn update_thread(
    State(state): State<SharedState>,
    caller: Caller,
    ChatPath(id): ChatPath<i64>,
    ChatJson(req): ChatJson<SettingsReq>,
) -> Response {
    let repo = ChatRepo::of(id);
    let missing = || err_json(StatusCode::NOT_FOUND, "not_found", "thread not found");
    // The patch and its checks first, without the thread's lock (review
    // W6-13): a device's tool labels are checked against its servers' live
    // lists, which may take the lazy-list budget, and the thread's turn
    // saves and a bound session's journal would wait on that. An id out of
    // the caller's reach is the 404 at once, before any lock.
    let Ok(Some(read)) = repo.thread_as(&state, &caller, id).await else {
        return missing();
    };
    // A device below `full` changes no setting of a thread with lmgw's admin
    // tools (L5's note): `apply_settings_patch` asks, as it does for an
    // ongoing folder's current thread.
    let mut t = read.clone();
    let mut seed =
        match apply_settings_patch(&state, &caller, &mut t, req.clone(), Change::Settings).await {
            Ok(seed) => seed,
            Err(refused) => return refused,
        };
    // Then read again and write under the thread's lock (review W5-2): the
    // settings are written whole, so an attach of the self-admin toolset
    // (which holds the same lock) lands before this read or after the
    // write, never between them; its `thread_as` is the re-check of the
    // device's reach. A thread that changed since the first read gets the
    // patch again, on what it is now (rare: its checks then run under the
    // lock).
    let hold = state.chat_live.hold(id).await;
    let Ok(Some(now)) = repo.thread_as(&state, &caller, id).await else {
        return missing();
    };
    if !same_thread(&read, &now) {
        t = now;
        seed = match apply_settings_patch(&state, &caller, &mut t, req, Change::Settings).await {
            Ok(seed) => seed,
            Err(refused) => return refused,
        };
    }
    let written = write_flipping(&state, repo, &t, seed, &caller, hold).await;
    match written {
        Ok(w) => {
            t.voice = w.voice;
            let last = repo.last_message(&state, id).await.ok().flatten();
            let snap = state.snapshot();
            let verdict = chat_turn::continue_state(&snap, &t, last.as_ref());
            Json(json!({
                "ok": true,
                "continue": verdict,
                "voice": t.voice,
                "voice_resolved": chat_voice::resolve_shown(&state, &t).await,
            }))
            .into_response()
        }
        // The thread went away between the read above and this write (a
        // delete in another tab, a temporary chat discarded or kept): the
        // 404 of any missing thread.
        Err(GatewayError::NotFound(msg)) => err_json(StatusCode::NOT_FOUND, "not_found", msg),
        Err(e) => err_json(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

/// Write `t`'s settings, moving the thread out of every device's reach and
/// back as the write did it (L3, reviews W3-1, W4-3, W4-8): a write that
/// attaches the self-admin toolset stops the thread's live events reaching
/// devices before it commits, and takes the thread from them after it — a
/// session a device bound closes, a turn it runs there is cancelled, for
/// whatever bound in between too; one that takes it off gives the thread
/// back after it commits. The close follows the commit (the desktop
/// client's live check, 2026-10-07; `chat_live::voice`'s module doc): it
/// came before it, and a device that asked for its folder's current thread
/// on the close was handed the thread it had just lost. The flip is decided
/// from the store's own before and after, not from a read made earlier, and
/// the write holds the thread's lock — `hold`, taken by the caller before it
/// read `t` (review W5-2) — so a device's user message lands before it or is
/// re-checked after it.
///
/// A write that fails after the pre-commit step gives the thread back the
/// flag it really has (review W5-7): otherwise a plain thread would stay
/// "admin" for devices.
///
/// From the first step to the last on a task of its own, with the lock
/// (`chat_live::to_its_end`, the branch review's N-3): a client that hangs
/// up meanwhile leaves no thread hidden from devices after a write that
/// failed, nor an attach committed with a device's session still open.
pub(crate) async fn write_flipping(
    state: &SharedState,
    repo: ChatRepo,
    t: &ChatThread,
    seed: SeedWrite,
    caller: &Caller,
    hold: crate::web::chat_live::HistoryWrite,
) -> Result<crate::store::SettingsWritten, GatewayError> {
    let (state, t, caller) = (state.clone(), t.clone(), caller.clone());
    crate::web::chat_live::to_its_end(async move {
        let attaching = t
            .drives_self_admin()
            .then(|| Attaching::start(&state, repo, &t));
        let written = repo.update_settings(&state, &t, seed, &caller).await;
        // Both after the write under the lock (review W6-14): another
        // attaching write's pre-commit step, made once the lock is free, is
        // never undone by this one's.
        match &written {
            Ok(w) => {
                if attaching.is_some() || w.level_before != w.level_after {
                    state
                        .chat_live
                        .self_admin_changed(&state.snapshot(), t.id, w.level_after);
                }
            }
            Err(_) if attaching.is_some() => restore_flag(&state, repo, t.id).await,
            Err(_) => {}
        }
        if let Some(a) = attaching {
            a.done();
        }
        drop(hold);
        written
    })
    .await?
}

/// An attach's pre-commit step (`LiveTurns::self_admin_attaching`, review
/// W4-3) until its write took the step after it — the post-commit flip or
/// [`restore_flag`]. A guard: dropped before [`Self::done`], by a panic
/// inside the write, it gives the thread back its real flag on a task of
/// its own, under the thread's lock, which the write's own lock lets go of
/// as it unwinds. The narrowing used to stay until the gateway restarted.
/// Read from the store under the lock, the flag is never one another
/// attaching write is about to commit (review W6-14).
pub(crate) struct Attaching {
    state: Option<SharedState>,
    repo: ChatRepo,
    thread_id: i64,
}

impl Attaching {
    /// Take the pre-commit step for `t`, which attaches the toolset.
    pub(crate) fn start(state: &SharedState, repo: ChatRepo, t: &ChatThread) -> Self {
        state.chat_live.self_admin_attaching(t.id, t.reach_level());
        Self {
            state: Some(state.clone()),
            repo,
            thread_id: t.id,
        }
    }

    /// The write took the step after it: nothing is left to give back.
    pub(crate) fn done(mut self) {
        self.state = None;
    }
}

impl Drop for Attaching {
    fn drop(&mut self) {
        let Some(state) = self.state.take() else {
            return;
        };
        let (repo, id) = (self.repo, self.thread_id);
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                let _hold = state.chat_live.hold(id).await;
                restore_flag(&state, repo, id).await;
            });
        }
    }
}

/// Whether two reads of a thread found it the same: every field, as it
/// serializes.
pub(crate) fn same_thread(a: &ChatThread, b: &ChatThread) -> bool {
    serde_json::to_value(a).ok() == serde_json::to_value(b).ok()
}

/// After a write that failed past its pre-commit step (review W5-7): the
/// thread's real flag, as stored now. A thread that is gone has none to
/// give back.
pub(crate) async fn restore_flag(state: &SharedState, repo: ChatRepo, id: i64) {
    if let Ok(Some(t)) = repo.thread(state, id).await {
        if !t.drives_self_admin() {
            state
                .chat_live
                .self_admin_changed(&state.snapshot(), id, t.reach_level());
        }
    }
}

/// Lay settings patch `req` over `t` with every check the settings route
/// runs, in its order: a device's tool labels and knowledge bases (L5), the
/// reasoning and sampling overrides, the knowledge settings, the voice and
/// its aliases, the aliases a device writes (review W3-4), and last, when
/// the patch changed anything, whether a device below `full` may change a
/// thread with lmgw's admin tools (`chat_steer`, as `change` names the
/// write: the route's, or an ongoing folder's defaults reaching its current
/// thread). Whether the stored voice seed stays; a refusal is the route's
/// own response, and `t` is then partly patched — the caller drops it.
pub(crate) async fn apply_settings_patch(
    state: &SharedState,
    caller: &Caller,
    t: &mut ChatThread,
    req: SettingsReq,
    change: fn(i64) -> Change,
) -> Result<SeedWrite, Response> {
    let before = t.clone();
    let aliases_before: Vec<Option<String>> = chat_tool_write::thread_aliases(t)
        .iter()
        .map(|a| a.map(str::to_string))
        .collect();
    if let Some(written) = &req.mcp_tools {
        chat_tool_write::check(state, caller, written, &t.mcp_tools).await?;
    }
    if let Some(ids) = &req.kb_ids {
        chat_tool_write::check_kbs(state, caller, ids, &t.kb_ids).await?;
    }
    if let Some(v) = req.model_alias {
        t.model_alias = v;
    }
    if let Some(v) = req.system_prompt {
        t.system_prompt = v;
    }
    if let Some(v) = req.temperature {
        t.temperature = v;
    }
    if let Some(v) = req.max_tokens {
        t.max_tokens = v;
    }
    if let Some(v) = req.mcp_tools {
        t.mcp_tools = v;
    }
    if let Some(v) = req.reasoning_enabled {
        t.reasoning_enabled = v;
    }
    if let Some(v) = req.reasoning_effort {
        t.reasoning_effort = v;
    }
    if let Some(v) = req.reasoning_budget {
        t.reasoning_budget = v;
    }
    if let Some(v) = req.top_p {
        t.top_p = v;
    }
    if let Some(v) = req.top_k {
        t.top_k = v;
    }
    if let Some(v) = req.min_p {
        t.min_p = v;
    }
    if let Some(v) = req.repeat_penalty {
        t.repeat_penalty = v;
    }
    if let Some(v) = req.presence_penalty {
        t.presence_penalty = v;
    }
    if let Some(v) = req.frequency_penalty {
        t.frequency_penalty = v;
    }
    if let Some(v) = req.seed {
        t.seed = v;
    }
    if let Some(v) = req.stop {
        t.stop = v;
    }
    if let Err(msg) = chat_reasoning::check(t).and_then(|()| chat_sampling::check(t)) {
        return Err(err_json(StatusCode::BAD_REQUEST, "bad_request", msg));
    }
    if let Err(msg) =
        chat_knowledge::apply_settings(state, t, req.kb_ids, req.kb_mode, req.kb_budget_tokens)
            .await
    {
        return Err(err_json(StatusCode::BAD_REQUEST, "bad_request", msg));
    }
    let seed = match chat_voice::apply_thread_voice(state, t, req.voice).await {
        Ok(seed) => seed,
        Err(msg) => return Err(err_json(StatusCode::BAD_REQUEST, "bad_request", msg)),
    };
    // The model and voice aliases a device writes are within its own alias
    // scope (review W3-4); the ones the thread carried pass.
    let carried: Vec<Option<&str>> = aliases_before.iter().map(Option::as_deref).collect();
    chat_tool_write::check_aliases(state, caller, &chat_tool_write::thread_aliases(t), &carried)?;
    // A thread with lmgw's admin tools: its settings drive the owner's later
    // turns there, so a device below `full` reads them and does not change
    // them (L5's note); the same settings sent back are no change (V-12).
    if !same_thread(&before, t) {
        if let Some(refused) =
            chat_steer::refusal(state, caller, before.reach_level(), change(t.id)).await
        {
            return Err(refused);
        }
    }
    Ok(seed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{self, SeedWrite, ThreadMcp};

    /// The last review's follow-up: an attach that panics between its
    /// pre-commit step and the step after the write — here a write of the
    /// test's own, run as the routes run theirs (`to_its_end`) — gives the
    /// thread back its real flag, under the thread's lock. It used to stay
    /// narrowed until the gateway restarted.
    #[tokio::test]
    async fn an_attach_that_panics_before_its_commit_gives_the_thread_its_flag_back() {
        let state = crate::state::AppState::init_for_tests().await.unwrap();
        let id = store::create_chat_thread(&state.db, "m", "chat")
            .await
            .unwrap();
        let mut t = store::get_chat_thread(&state.db, id)
            .await
            .unwrap()
            .unwrap();
        t.mcp_tools = vec![ThreadMcp {
            server_label: "lmgw".into(),
            allowed_tools: None,
        }];
        let devices = |state: &SharedState| {
            state
                .chat_feed
                .live
                .now(crate::store::AdminThreads::Hidden)
                .turns
                .len()
        };
        let _turn = state
            .chat_feed
            .live
            .turn_started(id, 0, "device 'phone'".into(), false);
        assert_eq!(devices(&state), 1);
        let hold = state.chat_live.hold(id).await;
        let task = state.clone();
        let out = tokio::spawn(crate::web::chat_live::to_its_end(async move {
            let _hold = hold;
            let _attaching = Attaching::start(&task, ChatRepo::Db, &t);
            assert_eq!(devices(&task), 0, "narrowed before the commit");
            panic!("an injected failure between the narrowing and the commit");
        }))
        .await;
        assert!(out.is_err_and(|e| e.is_panic()));
        // The flag comes back on a task of its own, under the lock.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while devices(&state) != 1 {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the thread is the devices' again");
        drop(state.chat_live.hold(id).await);
    }

    /// Review W6-12 (W5-7): an attach whose write fails after its
    /// pre-commit flip gives the thread back the flag it has, under the
    /// thread's lock (W6-14): a device's live view of the thread is the
    /// plain thread's again.
    #[tokio::test]
    async fn a_failed_attach_gives_the_thread_its_flag_back() {
        let state = crate::state::AppState::init_for_tests().await.unwrap();
        let id = store::create_chat_thread(&state.db, "m", "chat")
            .await
            .unwrap();
        let mut t = store::get_chat_thread(&state.db, id)
            .await
            .unwrap()
            .unwrap();
        t.mcp_tools = vec![ThreadMcp {
            server_label: "lmgw".into(),
            allowed_tools: None,
        }];
        sqlx::query(
            "CREATE TRIGGER refuse_settings BEFORE UPDATE ON chat_threads \
             BEGIN SELECT RAISE(ABORT, 'refused for the test'); END",
        )
        .execute(&state.db)
        .await
        .unwrap();
        let hold = state.chat_live.hold(id).await;
        let written = write_flipping(
            &state,
            ChatRepo::Db,
            &t,
            SeedWrite::Keep,
            &Caller::Owner(None),
            hold,
        )
        .await;
        assert!(written.is_err(), "the write failed");
        // The lock was given back with the flag.
        drop(state.chat_live.hold(id).await);
        // A turn of the thread is the plain thread's for a device again.
        let turn = state
            .chat_feed
            .live
            .turn_started(id, 0, "the dashboard".into(), false);
        assert_eq!(
            state
                .chat_feed
                .live
                .now(crate::store::AdminThreads::Hidden)
                .turns
                .len(),
            1
        );
        drop(turn);

        // The same attach landing keeps the thread from devices.
        sqlx::query("DROP TRIGGER refuse_settings")
            .execute(&state.db)
            .await
            .unwrap();
        let hold = state.chat_live.hold(id).await;
        write_flipping(
            &state,
            ChatRepo::Db,
            &t,
            SeedWrite::Keep,
            &Caller::Owner(None),
            hold,
        )
        .await
        .unwrap();
        let _turn = state
            .chat_feed
            .live
            .turn_started(id, 0, "the dashboard".into(), false);
        assert!(state
            .chat_feed
            .live
            .now(crate::store::AdminThreads::Hidden)
            .turns
            .is_empty());
    }
}
