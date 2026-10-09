//! What a device may write into a thread or a folder's defaults (client-apps
//! design L5): tool labels and knowledge bases within its own tool scope.
//!
//! **Tool labels** (`mcp_tools`, a thread's settings and a folder's
//! defaults). Each entry a device writes must keep every tool it exposes now
//! within the device key's tool scope (`ToolScope::admits`):
//! - a registered server's live list, as the resolver connects and lists it
//!   with the entry's own `allowed_tools`;
//! - a built-in toolset's names (`docs`, `kb`);
//! - a server that cannot list now (not connected, failing, disabled) only
//!   by its namespace: a prefixed one when the scope admits every name
//!   under `<prefix>__`; a bare one cannot be checked and is refused, saying
//!   so;
//! - `lmgw`, the self-admin toolset, only by a device whose admin tools are
//!   at `full` (`ApiKey::self_admin` capped by the gateway's level, as
//!   stored; the branch review's verification V-4): a thread or folder with
//!   it steers the owner's later turns, which run the write tools at the
//!   gateway's level, so a device below `full` uses the ones the owner made
//!   and never makes one (`403 chat_toolset_needs_full`); at `off` the label
//!   is out of its reach. Its own tool scope narrows the label where it
//!   names the `lmgw__` namespace, and an entry that leaves it none of the
//!   tools is refused (review P-6). What its tools may then do is the
//!   self-admin level's.
//!
//! A scope with no list of its own (`all`, `ToolScope::narrows`) admits
//! every tool but `lmgw`'s, so nothing else is listed or refused for it.
//!
//! An entry the thread or folder already carries, unchanged, was not written
//! by the device: a client that sends the whole list back is not refused for
//! what the thread already carries.
//!
//! **Knowledge bases** (the review's W2-4, decided by the owner 2026-10-06).
//! A device reaches the owner's bases as the `kb` toolset a key's scope
//! bounds, never around it: a base it names (a thread's `kb_ids`, a folder's
//! default, a message's `#` picks) needs `kb__search` within its scope —
//! what an auto-mode retrieval is. A base already named is not checked again.
//!
//! **`require_approval`** is checked for every writer, the owner too: an
//! entry the write changes must read as `/v1/responses` reads it, and no
//! two entries may name the same server — by its tool prefix and by its
//! name, say — where the write changed one of them (`400 bad_request`
//! otherwise). Entries carried unchanged are not checked again: a value
//! stored before these rules must not block an unrelated write.
//!
//! A device (any non-owner) only tightens it: a write that makes a server
//! require approval for fewer calls than the stored rule or the owner's
//! floor is `403 approval_loosen_refused` (owner's decision, 2026-10-09;
//! the `approval` module). Checked after the tool scope, so a label out of
//! the device's reach is answered as such — the two-entries `400` too,
//! which would otherwise say the server exists. The owner's writes set the
//! floor ([`owner_floor`]); a turn applies it again
//! (`mcp::exec::ApprovalFloor`).
//!
//! A refusal is `403 tool_label_out_of_scope`, naming the label and the
//! tool — or, for a base, the tool alone, never the base (review W3-5) — and
//! nothing is written. The owner's writes are not checked:
//! the owner attached them.

mod approval;

use axum::http::StatusCode;
use axum::response::Response;

use super::chat::err_json;
use super::chat_caller::Caller;
use super::chat_steer;
use crate::config::SelfAdmin;
use crate::mcp::exec::{list_label, server_label, DOCS_LABEL, KB_LABEL, SELF_ADMIN_LABEL};
use crate::mcp::scope::ToolScope;
use crate::mcp::spec::{ApprovalRule, McpToolSpec};
use crate::state::SharedState;
use crate::store::ThreadMcp;

/// The code every refusal here carries (§1.8).
pub(super) const CODE: &str = "tool_label_out_of_scope";

/// The tool a knowledge base is read through (module doc).
pub(super) const KB_READ: &str = "kb__search";

fn refuse(message: String) -> Response {
    err_json(StatusCode::FORBIDDEN, CODE, message)
}

/// Check the `mcp_tools` entries `caller` writes, against what was stored
/// `before`, the owner's approval `floor` and, for a thread, its `folder`
/// (module doc).
pub(super) async fn check(
    state: &SharedState,
    caller: &Caller,
    written: &[ThreadMcp],
    before: &[ThreadMcp],
    floor: &[ThreadMcp],
    folder: Option<i64>,
) -> Result<(), Response> {
    // `require_approval` reads as `/v1/responses` reads it, for every
    // writer (client-apps design §6.1): an unknown value, or a `read_only`
    // filter lmgw cannot honour, is refused rather than stored. Only for
    // the entries this write changes: one stored before these rules, sent
    // back unchanged with an unrelated field, is no new value (a turn reads
    // one that does not parse as "always").
    for entry in written.iter().filter(|e| !before.contains(e)) {
        if let Err(why) = crate::mcp::spec::parse_require_approval(
            entry.require_approval.as_ref(),
            &entry.server_label,
        ) {
            return Err(err_json(StatusCode::BAD_REQUEST, "bad_request", why));
        }
    }
    let duplicates = || {
        approval::duplicates(&state.snapshot(), written, before)
            .map_err(|why| err_json(StatusCode::BAD_REQUEST, "bad_request", why))
    };
    if !caller.is_device() {
        return duplicates();
    }
    let scope = caller.scope(state).await;
    for entry in written.iter().filter(|e| !before.contains(e)) {
        if entry.server_label.trim() == SELF_ADMIN_LABEL {
            if let Some(refused) = attach_refusal(state, caller).await {
                return Err(refused);
            }
        }
        if let Some(why) = refusal(state, &scope, entry).await {
            return Err(refuse(why));
        }
    }
    // After the scope: two labels for one server a device cannot reach
    // would tell it that server exists.
    duplicates()?;
    // A device only tightens `require_approval` (module `approval`).
    approval::check(state, written, before, floor, folder).await
}

/// The approval floor after the owner's write of `written` over `before`
/// (the `approval` module): what a thread's settings and a folder's
/// defaults store when the owner writes their tools.
pub(super) fn owner_floor(
    state: &SharedState,
    written: &[ThreadMcp],
    before: &[ThreadMcp],
    floor: &[ThreadMcp],
) -> Vec<ThreadMcp> {
    approval::owner_floor(&state.snapshot(), written, before, floor)
}

/// A device attaching `lmgw` (module doc): its admin tools must be at
/// `full`, read from its key row and the stored self-admin level, not the
/// snapshot `scope` was read from (review G-6): between a level's commit and
/// the snapshot's reload, the snapshot still allows an attach the level no
/// longer does. A read that fails refuses.
async fn attach_refusal(state: &SharedState, caller: &Caller) -> Option<Response> {
    match chat_steer::stored_level(state, caller).await? {
        Ok(SelfAdmin::Full) => None,
        Ok(SelfAdmin::Off) => Some(refuse(format!(
            "'{SELF_ADMIN_LABEL}': the self-admin toolset changes the gateway's own \
             configuration, and this device is not allowed lmgw's admin tools (its own level, \
             or the gateway's self-admin level that caps it, is off)"
        ))),
        Ok(level) => Some(err_json(
            StatusCode::FORBIDDEN,
            chat_steer::CODE,
            format!(
                "'{SELF_ADMIN_LABEL}': attaching lmgw's admin tools needs this device's admin \
                 tools at full, and they are {} (its own level, or the gateway's self-admin \
                 level that caps it). A thread or folder with these tools steers later turns \
                 there that may change lmgw's configuration; this device may use the ones \
                 that already carry them. Nothing was changed. The device's level is set on \
                 its row under Usage → Devices",
                chat_steer::level_words(level)
            ),
        )),
        Err(e) => Some(err_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!(
                "'{SELF_ADMIN_LABEL}': whether this device may attach lmgw's admin tools could \
                 not be read ({e}), so nothing was changed; try again"
            ),
        )),
    }
}

/// Why `entry` is out of `scope`'s reach, or `None` when every tool it
/// exposes now is within it.
async fn refusal(state: &SharedState, scope: &ToolScope, entry: &ThreadMcp) -> Option<String> {
    let label = entry.server_label.trim();
    if label == SELF_ADMIN_LABEL {
        if !scope.self_admin() {
            return Some(format!(
                "'{label}': the self-admin toolset changes the gateway's own configuration, and \
                 this device is not allowed lmgw's admin tools (its own level, or the \
                 gateway's self-admin level that caps it, is off)"
            ));
        }
        // The device's own patterns narrow the label where they name its
        // namespace (review P-6): an entry they leave no tool of is refused
        // now, rather than found empty at every turn. Judged against the
        // whole catalog: the self-admin level in Settings bounds what the
        // tools may do, and the owner may raise it.
        let spec = McpToolSpec {
            server_label: label.to_string(),
            allowed_tools: entry.allowed_tools.clone(),
            require_approval: ApprovalRule::Never,
        };
        let any = crate::mcp::selfadmin::list(crate::config::SelfAdmin::Full)
            .iter()
            .filter_map(|t| t.get("name").and_then(serde_json::Value::as_str))
            .filter(|name| {
                let short = name
                    .strip_prefix(crate::mcp::selfadmin::PREFIX)
                    .unwrap_or(name);
                spec.allows(name, short)
            })
            .any(|name| scope.admits(name));
        return (!any).then(|| {
            format!(
                "'{label}': none of lmgw's admin tools{} are within the tool scope of {}",
                if entry.allowed_tools.is_some() {
                    " this entry allows"
                } else {
                    ""
                },
                scope.describe()
            )
        });
    }
    let builtin = label == DOCS_LABEL || label == KB_LABEL;
    let snap = state.snapshot();
    let named = snap
        .mcp_servers
        .values()
        .find(|s| server_label(s) == label)
        .or_else(|| snap.mcp_servers.values().find(|s| s.name == label));
    // A scope with no list of its own admits every tool the label can ever
    // expose: nothing to list, nothing to connect (a bare server that is not
    // connected included — it cannot be out of such a scope's reach). Except
    // another device's hosted label, which such a scope holds back (L16,
    // review W3-9): that one is checked below, and only that one.
    let narrows = match named {
        Some(s) => scope.narrows_for(s),
        None => scope.narrows(),
    };
    if !narrows {
        return None;
    }
    // A registered server the device can never use is answered as an
    // unknown label is, before anything connects, and neither names any
    // other label. Not an oracle-free answer (review W3-18): a bare server
    // within its reach that lists nothing now says so, and a reachable
    // server's first out-of-scope tool is named — what `/v1/mcp/servers`
    // already shows the same device.
    let server = named.filter(|s| scope.may_reach(s)).cloned();
    if !builtin && server.is_none() {
        return Some(format!(
            "'{label}': no MCP server with this label is within the tool scope of {}",
            scope.describe()
        ));
    }
    let spec = McpToolSpec {
        server_label: label.to_string(),
        allowed_tools: entry.allowed_tools.clone(),
        require_approval: ApprovalRule::Never,
    };
    // Every tool the entry exposes now, whoever asks: the gateway's own reach
    // lists them, and the device's scope then judges each one.
    match list_label(state, &spec, &ToolScope::gateway()).await {
        Ok(listed) => listed
            .tools
            .iter()
            .find(|t| !scope.admits(&t.def.name))
            .map(|t| format!("'{label}': {}", scope.refusal(&t.def.name))),
        Err(e) => match server.as_ref().map(|s| s.tool_prefix.trim().to_string()) {
            // A built-in toolset that offers nothing now: nothing it
            // exposes is out of reach.
            None => Some(format!("'{label}': {}", e.message())),
            Some(prefix) if prefix.is_empty() => Some(format!(
                "'{label}' lists no tools right now (not connected, or disabled), and a server \
                 without a tool prefix can only be checked by what it lists — connect it, or \
                 let the owner attach it"
            )),
            Some(prefix) if scope.admits_namespace(&prefix) => None,
            Some(prefix) => Some(format!(
                "'{label}' lists no tools right now, and the tool scope of {} does not admit \
                 every tool under '{prefix}__' — connect it so its tools can be checked, or \
                 narrow allowed_tools",
                scope.describe()
            )),
        },
    }
}

/// Check the aliases `caller` writes — a thread's or a folder default's
/// model, its voice's speech-to-text and text-to-speech — against a device
/// key's alias scope (review W3-4): the owner's later turns, dictation and
/// read-aloud in that thread go to them, and a device must not choose an
/// alias for them it may not use itself. An alias already carried (in
/// `before`) passes, as a label does; empty is no alias. A refusal is the
/// key's own `403 key_scope`, and nothing is written. Not counted: a write
/// is no model call.
pub(super) fn check_aliases(
    state: &SharedState,
    caller: &Caller,
    written: &[Option<&str>],
    before: &[Option<&str>],
) -> Result<(), Response> {
    if !caller.is_device() {
        return Ok(());
    }
    let snap = state.snapshot();
    let carried: Vec<&str> = before.iter().flatten().map(|a| a.trim()).collect();
    for alias in written.iter().flatten().map(|a| a.trim()) {
        if alias.is_empty() || carried.contains(&alias) {
            continue;
        }
        if let Err(e) = caller.alias_in_scope(&snap, alias) {
            return Err(err_json(e.http_status(), e.code(), e.to_string()));
        }
    }
    Ok(())
}

/// A thread's aliases, as [`check_aliases`] reads them.
pub(super) fn thread_aliases(t: &crate::store::ChatThread) -> [Option<&str>; 3] {
    [
        Some(t.model_alias.as_str()),
        t.voice.asr_alias.as_deref(),
        t.voice.tts_alias.as_deref(),
    ]
}

/// A folder's default aliases, as [`check_aliases`] reads them.
pub(super) fn default_aliases(d: &crate::store::ThreadDefaults) -> [Option<&str>; 3] {
    let voice = d.voice.as_ref();
    [
        d.model_alias.as_deref(),
        voice.and_then(|v| v.asr_alias.as_deref()),
        voice.and_then(|v| v.tts_alias.as_deref()),
    ]
}

/// Check the knowledge bases `caller` names, against the ones already
/// named (module doc).
pub(super) async fn check_kbs(
    state: &SharedState,
    caller: &Caller,
    written: &[i64],
    before: &[i64],
) -> Result<(), Response> {
    if !caller.is_device() {
        return Ok(());
    }
    if written.iter().all(|id| before.contains(id)) {
        return Ok(());
    }
    let scope = caller.scope(state).await;
    if scope.admits(KB_READ) {
        return Ok(());
    }
    // Neither the base's name nor whether it exists (review W3-5): a device
    // that may not search the bases may not list them by probing ids
    // either. Run before the existence check, so a known id and an unknown
    // one answer alike.
    Err(refuse(format!(
        "a knowledge base is read as {}",
        scope.refusal(KB_READ)
    )))
}

/// Whether `caller` may read the owner's knowledge bases in a turn (module
/// doc): `Err` is the note a skipped retrieval carries.
pub(super) async fn kb_reach(state: &SharedState, caller: &Caller) -> Result<(), String> {
    if !caller.is_device() {
        return Ok(());
    }
    let scope = caller.scope(state).await;
    if scope.admits(KB_READ) {
        Ok(())
    } else {
        Err(format!(
            "the knowledge bases were not searched: a knowledge base is read as {}",
            scope.refusal(KB_READ)
        ))
    }
}
