//! Chat — the user-facing conversation surface (airy density), including
//! Admin Chat (kind == "admin", wired server-side to the lmgw__* tools).
//! Talks to the existing /chat/api/* JSON plane; streaming rides
//! [`super::chat_stream`]. Assistant markdown is rendered with pulldown-cmark
//! (single-user local app — same no-sanitizer trust model as the old UI).

mod composer;
/// The thread settings drawer: its draft, a save applied, the drawer.
mod drawer;
/// The thread list: dated groups, the filter, folders and pins.
mod list;
/// One message of the transcript, and the stats row under a reply.
mod msg_view;
/// The narrow layout: the side panels as drawers below 520 px.
mod narrow;
mod open;
/// The Chat component.
mod page;
mod settings_rebase;
mod stats;
mod tail;
mod wire;

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::chat::{Ack, ContinueState, MessageTask, ThreadTask};
use lmgw_api_types::chat_approvals::ApprovalRequest;
use lmgw_api_types::chat_threads::{AttachmentMeta, SettingsAck, TitleRequest};
use serde::Deserialize;
use serde_json::{json, Value};
use wasm_bindgen::{JsCast, JsValue};

use super::chat_actions::{ActionEnv, EditReq, MsgActions, MsgEditor, MsgOps};
use super::chat_approvals::{self, Approval, ApprovalBatch, ToolCardView};
use super::chat_attach::{
    draft_blockers, draft_from_attachment, draft_hints, new_draft, refresh_drafts, BlockerNote,
    DraftChip, DraftChips, HintNote, SentChip, ViewerMeta,
};
use super::chat_folders::{
    self, FolderDialogs, FolderEnv, FolderHeader, FolderInfo, FolderRow, NoFolderZone,
};
use super::chat_knowledge::{
    apply_kb, kb_badge, KbButton, KbChip, KbDraft, KbDraftChips, KbPick, KbPopover, KbUi,
};
use super::chat_media;
use super::chat_reasoning::reasoning_badge;
use super::chat_reply::{answer_label, UNSAVED_NOTE};
use super::chat_retrieval::{cite_html, cite_target, KbContext, RetrievalView};
use super::chat_sampling::{self, SamplingDraft, SamplingText};
use super::chat_search::{install_reveal, MessageHits};
use super::chat_settings::{draft_patch, DraftErrors, SettingsFields};
use super::chat_sync::{self, Follow, ListRead, ModelPicks, OwnEdits, Rescue};
use super::chat_tasks::{self, TaskEnv};
use super::chat_temp::{self, TempBanner};
use super::chat_turn::{run_turn, Turn, TurnEnv};
use super::chat_voice::{self, MsgVoice, ThreadVoice, VoiceDraft, VoiceResolved};
use super::knowledge_source::SourceModal;
pub(crate) use drawer::{apply_settings, SettingsDraft};
#[cfg(test)]
pub(crate) use list::group_threads;
pub(crate) use list::{matches_query, matches_query_in, parse_utc_ts, thread_row, ListItem};
use lmgw_ui_kit::scope::{Latest, Scope};
pub use lmgw_ui_kit::widgets::tool_picker::ThreadMcp;
use lmgw_ui_kit::widgets::{
    use_dirty_guard, use_slash_focus, use_toasts, Drawer, MenuItem, Modal, ModalSize, ModelPicker,
    RowMenu, Side, SplitPane,
};
pub(super) use open::Opener;
use open::{Landed, OpenEnv};
pub use page::Chat;
pub(super) use tail::{at_end, scroll_down, unpin};
pub(super) use wire::post_thread;

/// A thread as the page holds it: built from the gateway's `Thread` and
/// `ThreadRow` (`wire`), without the two times of the row that its
/// `PartialEq` must not see (`chat_sync`'s `row_take` and `rows::merge`
/// decide on it). Read from JSON only for a folder's defaults
/// (`chat_folders::thread_from_defaults`).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ChatThread {
    pub id: i64,
    pub title: String,
    pub model_alias: String,
    pub system_prompt: String,
    pub temperature: Option<f64>,
    pub max_tokens: Option<i64>,
    pub kind: String,
    /// Registered MCP servers this thread attaches; their tools are resolved
    /// server-side at send time and run by the gateway's agent loop.
    pub mcp_tools: Vec<ThreadMcp>,
    /// Reasoning overrides, sent with every turn like the
    /// `x-lmgw-reasoning` / `-effort` / `-budget` headers; `None` = the
    /// model's own default.
    pub reasoning_enabled: Option<bool>,
    pub reasoning_effort: Option<String>,
    pub reasoning_budget: Option<i64>,
    /// Sampling overrides, sent with every turn beside the temperature;
    /// `None` (an empty `stop`) = the route's default.
    pub top_p: Option<f64>,
    pub top_k: Option<i64>,
    pub min_p: Option<f64>,
    pub repeat_penalty: Option<f64>,
    pub presence_penalty: Option<f64>,
    pub frequency_penalty: Option<f64>,
    pub seed: Option<i64>,
    pub stop: Vec<String>,
    /// The knowledge bases every turn draws on, how (`auto` | `tool`), and the
    /// auto-mode excerpt budget (`None` = the `chat_kb_budget_tokens` setting).
    pub kb_ids: Vec<i64>,
    pub kb_mode: String,
    pub kb_budget_tokens: Option<i64>,
    /// The catalog agent this thread was opened from (agent-catalog §2.5).
    /// `None` for every thread started from this page; the agent's Threads tab
    /// is this list filtered by it.
    pub agent_id: Option<String>,
    pub updated_at: String,
    /// Sits atop the list, never auto-archives (archive-pin-attachments §1).
    pub pinned: bool,
    /// `None` = active. Set the moment the sweep (or a manual Archive) files
    /// it away; a pin or a send into it clears this again server-side.
    pub archived_at: Option<String>,
    /// The folder it sits in (chat-complete §5); `None` = no folder.
    pub folder_id: Option<i64>,
    /// When the purge sweep takes it — `archived_at` + the purge setting,
    /// computed server-side. `None` while active, pinned, or purge is off.
    pub purge_at: Option<String>,
    /// In gateway memory only, never stored (chat-complete §7); its id is
    /// negative. Discarded when left.
    pub temporary: bool,
    /// Whether the last reply can be continued, and if not why (§3).
    #[serde(rename = "continue")]
    pub cont: Option<ContinueState>,
    /// The thread's own voice settings, and what its voice resolves to
    /// (chat-voice §2.2, §2.3).
    pub voice: ThreadVoice,
    pub voice_resolved: Option<VoiceResolved>,
    /// The personality profile the thread talks with (`None` = none, the
    /// "Default" of the picker).
    pub profile_id: Option<i64>,
    /// The image and audio tools' default models of its own (an absent
    /// field inherits the folder's, then Settings'; empty is none).
    pub media_defaults: lmgw_api_types::chat_media::MediaDefaults,
}

/// `GET /chat/api/threads[?archived=1]`: the list plus how many threads sit
/// in the *other* view, so the toolbar toggle can say "Archived (n)" without
/// a second round trip.
#[derive(Debug, Clone, Default)]
pub(super) struct ThreadsResponse {
    pub(super) threads: Vec<ChatThread>,
    pub(super) archived_count: i64,
    /// The in-memory temporary chats, in every mode (chat-complete §7).
    pub(super) temporary: Vec<ChatThread>,
    /// Every folder with its thread counts, in every mode (§5).
    pub(super) folders: Vec<FolderInfo>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(super) struct MsgRow {
    pub(super) id: i64,
    pub(super) role: String,
    pub(super) content: String,
    pub(super) reasoning: String,
    pub(super) prompt_tokens: Option<i64>,
    pub(super) completion_tokens: Option<i64>,
    pub(super) ir_messages: Option<String>,
    /// A reply's model (the alias asked for) and, when another answered, that
    /// alias.
    pub(super) model: Option<String>,
    pub(super) answered_by: Option<String>,
    /// A reply a fallback that cannot see answered: what it got in the
    /// images' place, in a sentence.
    pub(super) images_note: Option<String>,
    pub(super) attachments: Vec<AttachmentMeta>,
    /// A user message's own knowledge bases and what they retrieved.
    pub(super) kb_refs: Vec<i64>,
    pub(super) context: Option<KbContext>,
    /// How the turn was spoken (chat-voice §3); `None`: typed.
    #[serde(deserialize_with = "chat_voice::tolerant_voice")]
    pub(super) voice: Option<MsgVoice>,
    /// Where a user message's client said it was written (chat-plane
    /// design §9.10); `None`: it said nothing.
    pub(super) origin: Option<String>,
    /// The calls a gated reply still waits on (client-apps design §6.2).
    pub(super) pending_approvals: Vec<ApprovalRequest>,
    /// A late MCP task result's facts, on a row of role `tool` (MCP Tasks
    /// design §2.2; `chat_tasks`).
    pub(super) task: Option<MessageTask>,
    /// When the row was stored (UTC, `YYYY-MM-DD HH:MM:SS`).
    pub(super) created_at: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(super) struct ThreadDetail {
    #[serde(deserialize_with = "wire::thread_of")]
    pub(super) thread: ChatThread,
    pub(super) messages: Vec<MsgRow>,
    /// Uploaded, not yet sent — the composer's chips on a reopened thread.
    pub(super) draft_attachments: Vec<AttachmentMeta>,
    /// Its MCP tasks still running or on their way in (`chat_tasks`).
    pub(super) tasks: Vec<ThreadTask>,
}

#[derive(Clone, Copy, PartialEq)]
pub(super) struct ToolCard {
    pub(super) index: i64,
    pub(super) name: RwSignal<String>,
    pub(super) args: RwSignal<String>,
    pub(super) output: RwSignal<String>,
    pub(super) is_error: RwSignal<bool>,
    pub(super) ms: RwSignal<Option<i64>>,
    pub(super) done: RwSignal<bool>,
    /// Whether the call waits for a decision, and the controls' state
    /// (client-apps design §6; `chat_approvals`).
    pub(super) approval: Approval,
    /// The call's id (its `ready` frame's, or the stored record's): what a
    /// media call's `progress` frames and its files name it by.
    pub(super) call_id: RwSignal<Option<String>>,
    /// A running image or audio call's stages, step and preview, from its
    /// `progress` frames (`chat_media`); `None` for every other call.
    pub(super) media: RwSignal<Option<chat_media::MediaLive>>,
}

#[derive(Clone)]
pub(super) struct Msg {
    pub(super) key: u64,
    /// The stored row's id, once known: from the loaded row, the stream's
    /// `turn` frame (the user bubble) or its `done` frame (the reply). The
    /// message actions address the row by it (chat-complete §3).
    pub(super) db_id: RwSignal<Option<i64>>,
    pub(super) role: String,
    pub(super) content: RwSignal<String>,
    pub(super) reasoning: RwSignal<String>,
    pub(super) tools: RwSignal<Vec<ToolCard>>,
    pub(super) streaming: RwSignal<bool>,
    pub(super) tokens: RwSignal<Option<(i64, i64)>>,
    /// A user message's bound files, upload order — empty for the assistant.
    pub(super) attachments: RwSignal<Vec<AttachmentMeta>>,
    /// A user message's own knowledge bases (`#` picks).
    pub(super) kb_refs: RwSignal<Vec<i64>>,
    /// What the turn that answers a message retrieved — on the answer, so its
    /// `[n]` citations resolve against it.
    pub(super) context: RwSignal<Option<KbContext>>,
    /// A reply's model and, when another alias answered, that one
    /// (`chat_reply::answer_label`).
    pub(super) model: RwSignal<Option<String>>,
    pub(super) answered_by: RwSignal<Option<String>>,
    /// A fallback that cannot see answered: what it got in the images'
    /// place (`done.images_note`, stored with the reply).
    pub(super) images_note: RwSignal<Option<String>>,
    /// The reply was not stored (`done.saved` false): dimmed, Copy only.
    pub(super) unsaved: RwSignal<bool>,
    /// How the turn was spoken (chat-voice §3).
    pub(super) voice: RwSignal<Option<MsgVoice>>,
    /// Where a user message was written, as its client said and the model
    /// reads it (chat-plane design §9.10).
    pub(super) origin: RwSignal<Option<String>>,
    /// A late MCP task result's facts (a message of role `tool`).
    pub(super) task: RwSignal<Option<MessageTask>>,
    /// When the row was stored, once known (UTC as stored).
    pub(super) created_at: RwSignal<Option<String>>,
    /// The turn's model could not start on the graphics card: the 503's
    /// message, shown as its own card (`chat_machine`).
    pub(super) gpu_setup: RwSignal<Option<String>>,
}

pub(super) fn new_msg(key: u64, role: &str, content: String) -> Msg {
    Msg {
        key,
        db_id: RwSignal::new(None),
        role: role.to_string(),
        content: RwSignal::new(content),
        reasoning: RwSignal::new(String::new()),
        tools: RwSignal::new(Vec::new()),
        streaming: RwSignal::new(false),
        tokens: RwSignal::new(None),
        attachments: RwSignal::new(Vec::new()),
        kb_refs: RwSignal::new(Vec::new()),
        context: RwSignal::new(None),
        model: RwSignal::new(None),
        answered_by: RwSignal::new(None),
        images_note: RwSignal::new(None),
        unsaved: RwSignal::new(false),
        voice: RwSignal::new(None),
        origin: RwSignal::new(None),
        task: RwSignal::new(None),
        created_at: RwSignal::new(None),
        gpu_setup: RwSignal::new(None),
    }
}

/// One stored tool interaction recovered from a turn's IR — the pure half of
/// the replay, so the pairing is unit-testable off the DOM.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct IrTool {
    /// The call's id in the record (`tool_use.id`).
    pub(super) id: String,
    pub(super) name: String,
    pub(super) args: String,
    pub(super) output: Option<String>,
    pub(super) is_error: bool,
}

/// A stored agentic turn is IR messages (assistant `tool_use` parts + their
/// `tool_result` parts), not text. Fold them back into the shape the tool card
/// renders, pairing each call with its result by id — so a reopened Admin Chat
/// thread shows what it did, not just what it concluded. Port of the old app's
/// `toolsFromIr` (chat-app.js:269-307).
pub(super) fn tools_from_ir(raw: &str) -> Vec<IrTool> {
    let Ok(turn) = serde_json::from_str::<Vec<Value>>(raw) else {
        return Vec::new();
    };
    let mut calls: Vec<IrTool> = Vec::new();
    let mut by_id: Vec<(String, usize)> = Vec::new();
    for m in &turn {
        for p in m["content"].as_array().into_iter().flatten() {
            match p["type"].as_str().unwrap_or("") {
                "tool_use" => {
                    by_id.push((
                        p["id"].as_str().unwrap_or_default().to_string(),
                        calls.len(),
                    ));
                    calls.push(IrTool {
                        id: p["id"].as_str().unwrap_or_default().to_string(),
                        name: p["name"].as_str().unwrap_or("tool").to_string(),
                        args: match p.get("args") {
                            Some(v) if !v.is_null() => {
                                serde_json::to_string_pretty(v).unwrap_or_default()
                            }
                            _ => "{}".to_string(),
                        },
                        output: None,
                        is_error: false,
                    });
                }
                "tool_result" => {
                    let id = p["id"].as_str().unwrap_or_default();
                    // The first call of that id still without its result: a
                    // record may carry an id twice (one stored before ids
                    // were unique), each call's result after it.
                    if let Some((_, at)) = by_id
                        .iter()
                        .find(|(k, at)| k == id && calls[*at].output.is_none())
                    {
                        calls[*at].output = Some(flatten_tool_result(&p["content"]));
                        calls[*at].is_error = p["is_error"].as_bool().unwrap_or(false);
                    }
                }
                _ => {}
            }
        }
    }
    calls
}

/// Whether a stored turn's IR holds any tool call.
pub(super) fn has_tools(ir: &str) -> bool {
    !tools_from_ir(ir).is_empty()
}

/// Render IR tool-result blocks down to the text the card shows. Binary blocks
/// (image/audio) are named, not inlined — same as the old app; a stored file's
/// resource is named by its ref (`image att:57`), its picture or player shown
/// under the card (`chat_media`).
fn flatten_tool_result(blocks: &Value) -> String {
    blocks
        .as_array()
        .into_iter()
        .flatten()
        .map(|b| match b["type"].as_str().unwrap_or("") {
            "text" => b["text"].as_str().unwrap_or("").to_string(),
            "json" => serde_json::to_string(&b["value"]).unwrap_or_default(),
            "resource" => chat_media::resource_name(b),
            other => format!("[{other}]"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The open thread's id, untracked.
fn current_id_now(current: RwSignal<Option<ChatThread>>) -> Option<i64> {
    current.with_untracked(|c| c.as_ref().map(|t| t.id))
}

/// Hand a rendered-markdown element to the JS glue (assets/codeblocks.js) for
/// syntax highlighting and — once `settled` — the copy/preview toolbar.
///
/// Highlighting stays in JS deliberately: highlight.js is already vendored, and
/// the Rust alternative (syntect plus its grammar set) would add megabytes to
/// the wasm bundle. A missing glue module (still loading) is a no-op; the
/// effect that calls this re-runs on the next token and again on settle.
fn decorate_code(el: &web_sys::Element, settled: bool) {
    let Some(win) = web_sys::window() else { return };
    let Ok(f) = js_sys::Reflect::get(&win, &JsValue::from_str("lmgwDecorateCode")) else {
        return;
    };
    let Ok(f) = f.dyn_into::<js_sys::Function>() else {
        return;
    };
    let _ = f.call2(&JsValue::NULL, el.as_ref(), &JsValue::from_bool(settled));
}

#[derive(Clone, Default, PartialEq)]
pub(super) struct Stats {
    pub(super) live: bool,
    pub(super) server: bool,
    pub(super) ttft_ms: Option<f64>,
    pub(super) total_ms: Option<f64>,
    pub(super) tps: Option<f64>,
    pub(super) prefill: Option<f64>,
    pub(super) prompt_tokens: Option<i64>,
    pub(super) completion_tokens: Option<i64>,
    pub(super) cached: Option<i64>,
    pub(super) draft_n: Option<i64>,
    pub(super) draft_accepted: Option<i64>,
    pub(super) ctx_max: Option<i64>,
    /// The thread's reasoning overrides the answering route did not send
    /// (`enabled` | `effort` | `budget`).
    pub(super) ignored: Vec<String>,
    /// The turn's `reasoning_note`: the model reasoned although off was
    /// asked, in a sentence — the badge's title says it.
    pub(super) reasoning_note: Option<String>,
}

/// Shared with the docs playground, which renders the exact markdown a
/// `docs__query` caller receives.
pub(super) use super::chat_markdown::md_to_html;

/// Whether a drag/drop carries files at all — as opposed to, say, dragged
/// text or a link, which the composer must still accept as ordinary text
/// input rather than swallow (review nit: dragover/drop used to
/// `prevent_default` unconditionally, which broke dropping text).
fn drag_has_files(dt: &web_sys::DataTransfer) -> bool {
    dt.types().includes(&JsValue::from_str("Files"), 0)
}

/// Run `f` under the page's owner — what it creates then lives exactly as
/// long as the page — or not at all once the page is gone.
pub(super) fn in_owner<T>(owner: StoredValue<WeakOwner>, f: impl FnOnce() -> T) -> Option<T> {
    owner
        .try_with_value(WeakOwner::upgrade)
        .flatten()
        .map(|o| o.with(f))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ir_tool_calls_pair_with_their_results_by_id() {
        let ir = serde_json::json!([
            {"role": "assistant", "content": [
                {"type": "text", "text": "let me look"},
                {"type": "tool_use", "id": "a1", "name": "lmgw__status", "args": {"verbose": true}},
                {"type": "tool_use", "id": "a2", "name": "lmgw__models", "args": {}}
            ]},
            // Results arrive out of order — pairing is by id, not position.
            {"role": "tool", "content": [
                {"type": "tool_result", "id": "a2", "is_error": false,
                 "content": [{"type": "text", "text": "3 models"}]}
            ]},
            {"role": "tool", "content": [
                {"type": "tool_result", "id": "a1", "is_error": false,
                 "content": [{"type": "text", "text": "ok"}]}
            ]}
        ])
        .to_string();
        let tools = tools_from_ir(&ir);
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "lmgw__status");
        assert_eq!(tools[0].args, "{\n  \"verbose\": true\n}");
        assert_eq!(tools[0].output.as_deref(), Some("ok"));
        assert_eq!(tools[1].name, "lmgw__models");
        assert_eq!(tools[1].args, "{}");
        assert_eq!(tools[1].output.as_deref(), Some("3 models"));
        assert!(!tools[0].is_error && !tools[1].is_error);
    }

    #[test]
    fn ir_tool_results_carry_the_error_flag() {
        let ir = serde_json::json!([
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "x", "name": "lmgw__container", "args": {"action": "apply"}}
            ]},
            {"role": "tool", "content": [
                {"type": "tool_result", "id": "x", "is_error": true,
                 "content": [{"type": "text", "text": "refused: read only"}]}
            ]}
        ])
        .to_string();
        let tools = tools_from_ir(&ir);
        assert_eq!(tools.len(), 1);
        assert!(tools[0].is_error);
        assert_eq!(tools[0].output.as_deref(), Some("refused: read only"));
    }

    #[test]
    fn ir_result_blocks_flatten_text_json_and_binaries() {
        let ir = serde_json::json!([
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "b", "name": "shot", "args": null}
            ]},
            {"role": "tool", "content": [
                {"type": "tool_result", "id": "b", "content": [
                    {"type": "text", "text": "line one"},
                    {"type": "json", "value": {"n": 2}},
                    {"type": "image", "mime": "image/png", "data": "AAAA"}
                ]}
            ]}
        ])
        .to_string();
        let tools = tools_from_ir(&ir);
        // Missing/null args render as "{}"; binary blocks are named, not inlined.
        assert_eq!(tools[0].args, "{}");
        assert_eq!(
            tools[0].output.as_deref(),
            Some("line one\n{\"n\":2}\n[image]")
        );
    }

    #[test]
    fn ir_calls_without_a_result_stay_output_less() {
        let ir = serde_json::json!([
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "only", "name": "slow_tool", "args": {}}
            ]},
            // A result for a call that is not in this turn is ignored.
            {"role": "tool", "content": [
                {"type": "tool_result", "id": "stranger",
                 "content": [{"type": "text", "text": "orphan"}]}
            ]}
        ])
        .to_string();
        let tools = tools_from_ir(&ir);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].output, None);
    }

    #[test]
    fn ir_without_tool_parts_yields_no_cards() {
        let plain = serde_json::json!([
            {"role": "assistant", "content": [{"type": "text", "text": "just prose"}]}
        ])
        .to_string();
        assert!(tools_from_ir(&plain).is_empty());
        assert!(tools_from_ir("not json at all").is_empty());
        assert!(tools_from_ir("[]").is_empty());
    }
}
