//! Chat — the user-facing conversation surface (airy density), including
//! Admin Chat (kind == "admin", wired server-side to the lmgw__* tools).
//! Talks to the existing /chat/api/* JSON plane; streaming rides
//! [`super::chat_stream`]. Assistant markdown is rendered with pulldown-cmark
//! (single-user local app — same no-sanitizer trust model as the old UI).

use std::cell::Cell;

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_query_map;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use wasm_bindgen::{JsCast, JsValue};

use super::chat_actions::{ActionEnv, ContinueState, EditReq, MsgActions, MsgEditor, MsgOps};
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
use super::chat_reasoning::reasoning_badge;
use super::chat_reply::{answer_label, UNSAVED_NOTE};
use super::chat_retrieval::{cite_html, cite_target, KbContext, RetrievalView};
use super::chat_sampling::{self, SamplingDraft, SamplingText};
use super::chat_search::{install_reveal, MessageHits};
use super::chat_settings::{draft_patch, DraftErrors, SettingsFields};
use super::chat_temp::{self, TempBanner};
use super::chat_turn::{run_turn, Turn, TurnEnv};
use super::chat_voice::{self, MsgVoice, ThreadVoice, VoiceDraft, VoiceResolved};
use super::knowledge_source::SourceModal;
use crate::scope::Scope;
pub use crate::widgets::tool_picker::ThreadMcp;
use crate::widgets::{
    use_dirty_guard, use_slash_focus, use_toasts, MenuItem, Modal, ModalSize, ModelPicker, RowMenu,
    Side, SplitPane,
};

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
}

/// A file dropped, pasted or picked into a message — either uploaded and
/// already bound to a sent message, or still a draft on the composer
/// (chat-archive-pin-attachments §2).
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub(super) struct Attachment {
    pub(super) id: i64,
    /// "image" | "text" | "pdf" | "office" | "audio" — decided server-side
    /// from the bytes, never the name or the browser's guessed MIME.
    pub(super) kind: String,
    pub(super) name: String,
    pub(super) mime: String,
    pub(super) size: i64,
    /// A text-class PDF's choice, "text" | "images"; `None` = not applicable
    /// or not chosen yet.
    pub(super) mode: Option<String>,
    /// What extraction found (chat_attach::chip_facts).
    pub(super) meta: Value,
    pub(super) extracted_tokens: Option<i64>,
    /// Drafts only: why this cannot be sent to the thread's current model.
    /// `None` = the server has not said (a sent file, or a fresh upload).
    pub(super) blockers: Option<Vec<String>>,
    /// Drafts only: what this becomes on the way, sent all the same (an
    /// image as a placeholder for a fallback that cannot see).
    pub(super) hints: Option<Vec<String>>,
}

/// `GET /chat/api/threads[?archived=1]`: the list plus how many threads sit
/// in the *other* view, so the toolbar toggle can say "Archived (n)" without
/// a second round trip.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct ThreadsResponse {
    threads: Vec<ChatThread>,
    archived_count: i64,
    /// The in-memory temporary chats, in every mode (chat-complete §7).
    temporary: Vec<ChatThread>,
    /// Every folder with its thread counts, in every mode (§5).
    folders: Vec<FolderInfo>,
}

/// The open thread's settings as they are being edited. Held by the page,
/// not by the panel that shows them: the panel unmounts whenever it folds —
/// a click back in the transcript, Esc, a narrower window — and a system
/// prompt typed into it went with it (review code:A2, par:PAR-1). Re-seeded
/// when another thread opens, as before.
#[derive(Clone, Copy)]
pub(super) struct SettingsDraft {
    pub(super) sys: RwSignal<String>,
    pub(super) temp: RwSignal<String>,
    pub(super) max_tok: RwSignal<String>,
    /// `""` (the model's default), `"on"` or `"off"`.
    pub(super) think: RwSignal<String>,
    pub(super) effort: RwSignal<String>,
    pub(super) budget: RwSignal<String>,
    pub(super) sampling: SamplingDraft,
    pub(super) picked: RwSignal<Vec<ThreadMcp>>,
    pub(super) kb: KbDraft,
    pub(super) voice: VoiceDraft,
}

impl SettingsDraft {
    pub(super) fn new() -> Self {
        Self {
            sys: RwSignal::new(String::new()),
            temp: RwSignal::new(String::new()),
            max_tok: RwSignal::new(String::new()),
            think: RwSignal::new(String::new()),
            effort: RwSignal::new(String::new()),
            budget: RwSignal::new(String::new()),
            sampling: SamplingDraft::new(),
            picked: RwSignal::new(Vec::new()),
            kb: KbDraft::new(),
            voice: VoiceDraft::new(),
        }
    }

    pub(super) fn seed(&self, t: &ChatThread) {
        let text = SettingsText::of(t);
        self.sys.set(text.sys);
        self.temp.set(text.temp);
        self.max_tok.set(text.max_tok);
        self.think.set(text.think);
        self.effort.set(text.effort);
        self.budget.set(text.budget);
        self.sampling.seed(text.sampling);
        self.picked.set(t.mcp_tools.clone());
        self.kb.seed(t);
        self.voice.load(&t.voice);
    }

    /// The form as it stands (tracked).
    fn text(&self) -> SettingsText {
        SettingsText {
            sys: self.sys.get(),
            temp: self.temp.get(),
            max_tok: self.max_tok.get(),
            think: self.think.get(),
            effort: self.effort.get(),
            budget: self.budget.get(),
            sampling: self.sampling.text(),
        }
    }

    /// Does the form differ from the thread as stored? (tracked)
    fn differs_from(&self, t: &ChatThread) -> bool {
        self.text().differs(&SettingsText::of(t))
            || self.picked.with(|p| *p != t.mcp_tools)
            || self.kb.differs_from(t)
            || self.voice.differs_from(&t.voice)
    }
}

/// A settings patch the server took, applied to the thread as the page holds
/// it: only the keys the patch carries.
fn apply_settings(t: &mut ChatThread, body: &Value) {
    if let Some(v) = body.get("model_alias").and_then(Value::as_str) {
        t.model_alias = v.to_string();
    }
    if let Some(v) = body.get("system_prompt").and_then(Value::as_str) {
        t.system_prompt = v.to_string();
    }
    if let Some(v) = body.get("temperature") {
        t.temperature = v.as_f64();
    }
    if let Some(v) = body.get("max_tokens") {
        t.max_tokens = v.as_i64();
    }
    if let Some(v) = body.get("mcp_tools") {
        t.mcp_tools = serde_json::from_value(v.clone()).unwrap_or_default();
    }
    if let Some(v) = body.get("reasoning_enabled") {
        t.reasoning_enabled = v.as_bool();
    }
    if let Some(v) = body.get("reasoning_effort") {
        t.reasoning_effort = v.as_str().map(str::to_string);
    }
    if let Some(v) = body.get("reasoning_budget") {
        t.reasoning_budget = v.as_i64();
    }
    chat_sampling::apply(t, body);
    apply_kb(t, body);
}

/// A thread's settings as the form's text: prompt, temperature, max tokens,
/// and the three reasoning overrides.
#[derive(Clone, Debug, Default, PartialEq)]
struct SettingsText {
    sys: String,
    temp: String,
    max_tok: String,
    think: String,
    effort: String,
    budget: String,
    sampling: SamplingText,
}

impl SettingsText {
    fn of(t: &ChatThread) -> Self {
        Self {
            sys: t.system_prompt.clone(),
            temp: t.temperature.map(|v| v.to_string()).unwrap_or_default(),
            max_tok: t.max_tokens.map(|v| v.to_string()).unwrap_or_default(),
            think: match t.reasoning_enabled {
                Some(true) => "on".into(),
                Some(false) => "off".into(),
                None => String::new(),
            },
            effort: t.reasoning_effort.clone().unwrap_or_default(),
            budget: t
                .reasoning_budget
                .map(|v| v.to_string())
                .unwrap_or_default(),
            sampling: SamplingText::of(t),
        }
    }

    /// Compared as a save would send them: surrounding blanks are not an edit.
    /// Numbers by value — `0.70` saved is `0.7` stored, and `0512` is `512` —
    /// or the marker would stay on after a save that took them.
    fn differs(&self, b: &Self) -> bool {
        self.sys != b.sys
            || !same_number::<f64>(&self.temp, &b.temp)
            || !same_number::<i64>(&self.max_tok, &b.max_tok)
            || !same_number::<i64>(&self.budget, &b.budget)
            || self.think.trim() != b.think.trim()
            || self.effort.trim() != b.effort.trim()
            || self.sampling.differs(&b.sampling)
    }
}

/// Two boxes hold the same number, or — when either is not one — the same
/// text.
fn same_number<T: std::str::FromStr + PartialEq>(a: &str, b: &str) -> bool {
    match (a.trim().parse::<T>(), b.trim().parse::<T>()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a.trim() == b.trim(),
    }
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
    pub(super) attachments: Vec<Attachment>,
    /// A user message's own knowledge bases and what they retrieved.
    pub(super) kb_refs: Vec<i64>,
    pub(super) context: Option<KbContext>,
    /// How the turn was spoken (chat-voice §3); `None`: typed.
    #[serde(deserialize_with = "chat_voice::tolerant_voice")]
    pub(super) voice: Option<MsgVoice>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(super) struct ThreadDetail {
    pub(super) thread: ChatThread,
    pub(super) messages: Vec<MsgRow>,
    /// Uploaded, not yet sent — the composer's chips on a reopened thread.
    pub(super) draft_attachments: Vec<Attachment>,
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
    pub(super) attachments: RwSignal<Vec<Attachment>>,
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
    }
}

/// One stored tool interaction recovered from a turn's IR — the pure half of
/// the replay, so the pairing is unit-testable off the DOM.
#[derive(Debug, Clone, PartialEq)]
struct IrTool {
    name: String,
    args: String,
    output: Option<String>,
    is_error: bool,
}

/// A stored agentic turn is IR messages (assistant `tool_use` parts + their
/// `tool_result` parts), not text. Fold them back into the shape the tool card
/// renders, pairing each call with its result by id — so a reopened Admin Chat
/// thread shows what it did, not just what it concluded. Port of the old app's
/// `toolsFromIr` (chat-app.js:269-307).
fn tools_from_ir(raw: &str) -> Vec<IrTool> {
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
                    if let Some((_, at)) = by_id.iter().find(|(k, _)| k == id) {
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

/// Render IR tool-result blocks down to the text the card shows. Binary blocks
/// (image/audio/resource) are named, not inlined — same as the old app.
fn flatten_tool_result(blocks: &Value) -> String {
    blocks
        .as_array()
        .into_iter()
        .flatten()
        .map(|b| match b["type"].as_str().unwrap_or("") {
            "text" => b["text"].as_str().unwrap_or("").to_string(),
            "json" => serde_json::to_string(&b["value"]).unwrap_or_default(),
            other => format!("[{other}]"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Which thread a cold load opens: the `?t=` deep link always wins, even
/// into a thread the (active-only) list just loaded does not carry — it may
/// be archived, since a cold load only fetches the active list (review
/// finding #7: this used to open the first active thread and rewrite the
/// URL out from under an archived deep link) — else the first (most recently
/// active) one, the old app's `firstUpdated` rule (chat-app.js:371-375). A
/// deep link naming a thread that turns out not to exist at all still
/// surfaces as `open_thread`'s own error toast rather than silently landing
/// on some other conversation.
fn seed_thread(param: Option<i64>, threads: &[ChatThread]) -> Option<i64> {
    param.or_else(|| threads.first().map(|t| t.id))
}

/// Keep the address bar on `/chat?t=<id>` (bare `/chat` with nothing open), so
/// a reload or a copied link returns to the same thread. Raw `replaceState`
/// rather than the router's `navigate`: this fires on every thread switch,
/// including mid-stream, and must not re-run route matching — the router picks
/// the location back up on `popstate`.
fn sync_url(id: Option<i64>) {
    let url = match id {
        Some(id) => format!("/chat?t={id}"),
        None => "/chat".to_string(),
    };
    if let Some(h) = web_sys::window().and_then(|w| w.history().ok()) {
        let _ = h.replace_state_with_url(&JsValue::NULL, "", Some(&url));
    }
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

thread_local! {
    /// At most one queued frame: a local model lands dozens of tokens between
    /// repaints, and each would otherwise register its own callback for the
    /// same single scroll. `Some(true)` means a forced scroll won the frame.
    static SCROLL_PENDING: Cell<Option<bool>> = const { Cell::new(None) };
}

/// Keep the tail of the conversation in view while it streams — the port of
/// the old app's `scrollDown` (chat-app.js:742-747), which the Leptos rewrite
/// dropped along with its `.messages` pane.
///
/// `force` (opening a thread, sending) always lands at the bottom. Otherwise
/// the pane only follows while the reader is already within ~120px of it: past
/// that they have deliberately scrolled up to read something, and yanking them
/// back down every token is worse than letting the tail run on.
///
/// Measured inside a `requestAnimationFrame`, not at the call site: Leptos
/// flushes the DOM on the microtask queue, so the element a token produced does
/// not exist yet when the event handler returns, and `scrollHeight` read there
/// is the previous frame's.
pub(super) fn scroll_down(force: bool) {
    let already_queued = SCROLL_PENDING.with(|p| {
        let prev = p.get();
        p.set(Some(prev.unwrap_or(false) || force));
        prev.is_some()
    });
    if already_queued {
        return;
    }
    request_animation_frame(move || {
        let force = SCROLL_PENDING.with(|p| p.take()).unwrap_or(false);
        let Some(pane) = document().get_element_by_id("chat-scroll") else {
            return;
        };
        let height = pane.scroll_height();
        if force || height - pane.scroll_top() - pane.client_height() < 120 {
            pane.set_scroll_top(height);
        }
    });
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

impl Stats {
    /// Map a llama.cpp `timings` block (cache_n prompt tokens were KV-reused;
    /// full prompt = processed + cached). First-token decode rates are noise.
    pub(super) fn from_timings(t: &Value, ctx_max: Option<i64>) -> Stats {
        let cached = t["cache_n"].as_i64().unwrap_or(0);
        let predicted_n = t["predicted_n"].as_i64().unwrap_or(0);
        Stats {
            server: true,
            ctx_max,
            prefill: t["prompt_per_second"].as_f64().filter(|v| v.is_finite()),
            tps: if predicted_n >= 2 {
                t["predicted_per_second"].as_f64().filter(|v| v.is_finite())
            } else {
                None
            },
            prompt_tokens: Some(t["prompt_n"].as_i64().unwrap_or(0) + cached),
            completion_tokens: t["predicted_n"].as_i64(),
            cached: (cached > 0).then_some(cached),
            draft_n: t["draft_n"].as_i64().filter(|n| *n > 0),
            draft_accepted: t["draft_n_accepted"].as_i64(),
            ..Default::default()
        }
    }
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

#[component]
pub fn Chat() -> impl IntoView {
    let toasts = use_toasts();

    let threads = RwSignal::new(Vec::<ChatThread>::new());
    let current = RwSignal::new(None::<ChatThread>);
    let msgs = RwSignal::new(Vec::<Msg>::new());
    let next_key = StoredValue::new(0u64);
    let catalog = crate::catalog::use_model_catalog();
    let model_sel = RwSignal::new(String::new());
    let composer = RwSignal::new(String::new());
    // Knowledge (chat-complete §9.3): the bases, the one source viewer, and
    // the `#` picks of the message being written.
    let kbui = KbUi::provide(Scope::new());
    // This window's audio devices and echo mode (chat-voice §2.4).
    chat_voice::provide_voice_devices();
    let draft_kbs = RwSignal::new(Vec::<i64>::new());
    let composer_ta: NodeRef<leptos::html::Textarea> = NodeRef::new();
    let composer_box: NodeRef<leptos::html::Div> = NodeRef::new();
    let kb_pick = KbPick::new(draft_kbs, composer, composer_ta);
    // Dictation, read-aloud and their status line (chat-voice WP7).
    let page_voice = chat_voice::provide_page_voice(current, composer, composer_ta);
    // The thread whose reply is streaming, not just "something is": the
    // owner may open another thread meanwhile, and that one's Stop must not
    // abort this stream, nor its stats row show this stream's numbers.
    let streaming = RwSignal::new(None::<i64>);
    // The streaming reply's own message, to put back under its thread when
    // the owner returns to it before the server has stored it.
    let live = StoredValue::new(None::<(i64, Msg)>);
    // The last reply's numbers, tagged with its thread.
    let stats = RwSignal::new(None::<(i64, Stats)>);
    let aborter = StoredValue::new(None::<web_sys::AbortController>);
    // The list's read: `None` while the first is out, the error of the last
    // one that failed. A failed read is not "no conversations" (code:A3).
    let threads_state = RwSignal::new(None::<Result<(), String>>);
    // The toolbar's "Archived (n)" toggle: which list is fetched and shown.
    // The count is carried by every list response, active or archived, so the
    // toggle's label is right even while looking at the other one.
    let view_archived = RwSignal::new(false);
    // A search hit's message, to scroll to and flash once its thread's
    // transcript is in (`?m=` on a cold load, or a click on a hit).
    let focus = RwSignal::new(None::<i64>);
    install_reveal(focus, msgs);
    let archived_count = RwSignal::new(0i64);
    // The gateway's in-memory temporary chats (chat-complete §7), listed in
    // their own group above the dated ones; and Keep's write in flight.
    let temp_threads = RwSignal::new(Vec::<ChatThread>::new());
    let keep_busy = RwSignal::new(false);
    // The folders (chat-complete §5), read with every thread list; the folder
    // UI's own state and actions are `folders`, made once the callbacks they
    // need exist.
    let folder_list = RwSignal::new(Vec::<FolderInfo>::new());
    // The open temporary chat's id (0 = none), readable from `on_cleanup`,
    // which must not touch signals the disposal may already have taken.
    let open_temp = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));
    // The composer's chips: uploaded (or uploading) attachments not yet sent,
    // reseeded from `draft_attachments` whenever a thread opens.
    let draft_attachments = RwSignal::new(Vec::<DraftChip>::new());
    let drag_over = RwSignal::new(false);
    let file_input: NodeRef<leptos::html::Input> = NodeRef::new();
    // A chip's own click: the image full-size, or a text file's content, in a
    // shared Modal rather than one per message.
    let viewer_open = RwSignal::new(false);
    let viewer_is_image = RwSignal::new(false);
    let viewer_name = RwSignal::new(String::new());
    let viewer_src = RwSignal::new(String::new());
    // The open non-image attachment: its header (tokens, Download original).
    let viewer_att = RwSignal::new(None::<Attachment>);
    let viewer_loading = RwSignal::new(false);
    // Bumped on every open; a text fetch only applies its answer if it is
    // still the current one (review nit: opening a text attachment then an
    // image before the text fetch returns put the text into the image
    // modal, since both share `viewer_src`).
    let viewer_gen = StoredValue::new(0u64);
    // Signals created inside event handlers die with that handler's view
    // scope (the Send button disposes itself when it flips to Stop) — new
    // per-message signals must be owned by the component instead. Held
    // weakly: this StoredValue lives in that owner's own arena, and a strong
    // handle there is a cycle that never let the page be disposed — every
    // visit to Chat used to stay in memory with its listeners, observers and
    // detached transcript (review code:C3).
    let owner = StoredValue::new(Owner::current().expect("component owner").downgrade());
    // Reads that answer after the page was left end there (review code:C4):
    // a late thread load would otherwise rewrite the next page's URL.
    let scope = Scope::new();

    let alloc_key = move || {
        let k = next_key.get_value();
        next_key.set_value(k + 1);
        k
    };

    // The active or the archived list, whichever the toolbar toggle shows.
    let threads_url = move || {
        if view_archived.get_untracked() {
            "/chat/api/threads?archived=1"
        } else {
            "/chat/api/threads"
        }
    };

    // A failed re-read keeps the list it had and says so above it.
    let refresh_threads = move || {
        scope.spawn(async move {
            match crate::api::get::<ThreadsResponse>(threads_url()).await {
                Ok(r) => {
                    threads.set(r.threads);
                    temp_threads.set(r.temporary);
                    folder_list.set(r.folders);
                    archived_count.set(r.archived_count);
                    threads_state.set(Some(Ok(())));
                }
                Err(e) => threads_state.set(Some(Err(e.to_string()))),
            }
        });
    };

    // Per-message signals are created here rather than in the calling task, so
    // they belong to the component and die with it (see `owner` above).
    let msgs_of_rows = move |rows: Vec<MsgRow>| -> Vec<Msg> {
        let mapped = in_owner(owner, || {
            // The retrieval of the user message an answer follows.
            let mut answering: Option<KbContext> = None;
            rows.into_iter()
                .map(|r| {
                    let m = new_msg(alloc_key(), &r.role, r.content);
                    m.db_id.set(Some(r.id));
                    if r.role == "user" {
                        answering = r.context;
                        m.kb_refs.set(r.kb_refs);
                    } else if answering.is_some() {
                        m.context.set(answering.clone());
                    }
                    m.reasoning.set(r.reasoning);
                    m.voice.set(r.voice);
                    m.model.set(r.model);
                    m.answered_by.set(r.answered_by);
                    m.images_note.set(r.images_note);
                    if !r.attachments.is_empty() {
                        m.attachments.set(r.attachments);
                    }
                    if let (Some(p), Some(c)) = (r.prompt_tokens, r.completion_tokens) {
                        m.tokens.set(Some((p, c)));
                    }
                    // Replay a stored agentic turn's tool calls as full cards,
                    // identical to the live ones.
                    if let Some(ir) = &r.ir_messages {
                        let cards: Vec<ToolCard> = tools_from_ir(ir)
                            .into_iter()
                            .enumerate()
                            .map(|(i, t)| ToolCard {
                                index: i as i64,
                                name: RwSignal::new(t.name),
                                args: RwSignal::new(t.args),
                                output: RwSignal::new(t.output.unwrap_or_default()),
                                is_error: RwSignal::new(t.is_error),
                                // Durations are not stored with the IR; the card
                                // shows "done" instead of a made-up number.
                                ms: RwSignal::new(None),
                                done: RwSignal::new(true),
                            })
                            .collect();
                        if !cards.is_empty() {
                            m.tools.set(cards);
                        }
                    }
                    m
                })
                .collect()
        });
        mapped.unwrap_or_default()
    };
    let load_msgs = move |rows: Vec<MsgRow>| msgs.set(msgs_of_rows(rows));

    // A message to reveal (search hit, `?m=`) that the open transcript does
    // not hold is dropped with a note, not waited for: a `focus` left set
    // would skip the scroll-to-end of every later thread open.
    let settle_focus = move || {
        if let Some(mid) = focus.get_untracked() {
            let has =
                msgs.with_untracked(|v| v.iter().any(|m| m.db_id.get_untracked() == Some(mid)));
            if !has {
                focus.set(None);
                toasts.warn("that message no longer exists");
            }
        }
    };

    let open_thread = move |id: i64| {
        // Already open: a no-op rather than a re-fetch that would overwrite
        // `draft_attachments` from the server's (upload-order) view of the
        // thread's drafts — which does not yet know about a chip whose
        // upload is still in flight, so clicking the open row dropped it
        // until the next reopen (review nit).
        if current.with_untracked(|c| c.as_ref().map(|t| t.id) == Some(id)) {
            return;
        }
        // Leaving a temporary chat throws it away, without a question — that
        // is what it is for. Only once the next thread has loaded: a failed
        // read keeps showing the temporary chat, so it must still exist.
        let left = current.with_untracked(|c| c.as_ref().filter(|t| t.temporary).map(|t| t.id));
        scope.spawn(async move {
            match crate::api::get::<ThreadDetail>(format!("/chat/api/threads/{id}")).await {
                Ok(d) => {
                    // A reply still streaming into the left chat is stopped
                    // first.
                    if let Some(left) = left {
                        if streaming.get_untracked() == Some(left) {
                            if let Some(a) = aborter.get_value() {
                                a.abort();
                            }
                        }
                        chat_temp::discard(left);
                        temp_threads.update(|v| v.retain(|t| t.id != left));
                    }
                    model_sel.set(d.thread.model_alias.clone());
                    current.set(Some(d.thread));
                    // A settled reply's numbers belong to the visit; a reply
                    // still streaming keeps its own until it is done.
                    let keep = streaming.get_untracked();
                    stats.update(|s| {
                        if s.as_ref().is_some_and(|(t, _)| Some(*t) != keep) {
                            *s = None;
                        }
                    });
                    load_msgs(d.messages);
                    // Back on the thread whose reply is streaming: the server
                    // stores the reply when it is done, so until then the
                    // live message is the only copy of it.
                    if keep == Some(id) {
                        if let Some((_, m)) = live.get_value().filter(|(t, _)| *t == id) {
                            msgs.update(|v| v.push(m));
                        }
                    }
                    // Reseed the composer's chips from this thread's own
                    // drafts — signals belong to the component (see `owner`).
                    let chips = in_owner(owner, || {
                        d.draft_attachments
                            .into_iter()
                            .map(|a| draft_from_attachment(alloc_key(), a))
                            .collect()
                    });
                    draft_attachments.set(chips.unwrap_or_default());
                    draft_kbs.set(Vec::new());
                    sync_url(Some(id));
                    settle_focus();
                    if focus.get_untracked().is_none() {
                        scroll_down(true);
                    }
                }
                Err(e) => {
                    // A reveal aimed at this thread has nowhere to land; left
                    // set it would skip the scroll of the next open.
                    focus.set(None);
                    toasts.err(e.to_string());
                }
            }
        });
    };

    // Deep link: `/chat?t=<id>` opens that thread on a cold load, otherwise the
    // first one in the list. Read untracked — the URL is rewritten on every
    // switch below, and re-reading it here would loop.
    let seed = use_query_map().with_untracked(|q| q.get("t").and_then(|v| v.parse::<i64>().ok()));
    focus.set(use_query_map().with_untracked(|q| q.get("m").and_then(|v| v.parse::<i64>().ok())));
    let first_load = move || {
        scope.spawn(async move {
            match crate::api::get::<ThreadsResponse>(threads_url()).await {
                Ok(r) => {
                    let open = seed_thread(seed, &r.threads);
                    threads.set(r.threads);
                    temp_threads.set(r.temporary);
                    folder_list.set(r.folders);
                    archived_count.set(r.archived_count);
                    threads_state.set(Some(Ok(())));
                    match open {
                        Some(id) => open_thread(id),
                        None => sync_url(None),
                    }
                }
                Err(e) => threads_state.set(Some(Err(e.to_string()))),
            }
        });
    };
    first_load();
    // Retry: the first read again while nothing is open, else a re-read.
    let retry_threads = Callback::new(move |()| {
        if current.with_untracked(Option::is_none) {
            first_load();
        } else {
            refresh_threads();
        }
    });

    // `kind` is "chat" | "admin", or "temporary" for a chat that is never saved.
    let new_thread = move |kind: &'static str| {
        let alias = model_sel.get_untracked();
        let body = if kind == "temporary" {
            json!({ "model_alias": alias, "kind": "chat", "temporary": true })
        } else {
            json!({ "model_alias": alias, "kind": kind })
        };
        spawn_local(async move {
            match crate::api::post::<ChatThread, _>("/chat/api/threads", &body).await {
                Ok(t) => {
                    let id = t.id;
                    // The page was left before the answer: nothing will show
                    // the chat, and a temporary one would never be discarded.
                    if !scope.alive() {
                        if id < 0 {
                            chat_temp::discard(id);
                        }
                        return;
                    }
                    refresh_threads();
                    open_thread(id);
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    // Deleting the open thread falls through to the next one (the old app's
    // rule), and with none left the URL goes back to bare /chat.
    let delete_thread = move |id: i64| {
        scope.spawn(async move {
            if let Err(e) =
                crate::api::post::<Value, _>(format!("/chat/api/threads/{id}/delete"), &json!({}))
                    .await
            {
                toasts.err(format!("deleting the conversation failed: {e}"));
                return;
            }
            let was_open = current.get_untracked().map(|t| t.id) == Some(id);
            // A failed re-read keeps the list as it was, less the deleted
            // row, rather than emptying it (code:A3).
            let list = match crate::api::get::<ThreadsResponse>(threads_url()).await {
                Ok(r) => {
                    threads_state.set(Some(Ok(())));
                    archived_count.set(r.archived_count);
                    temp_threads.set(r.temporary);
                    folder_list.set(r.folders);
                    r.threads
                }
                Err(e) => {
                    threads_state.set(Some(Err(e.to_string())));
                    let mut left = threads.get_untracked();
                    left.retain(|t| t.id != id);
                    temp_threads.update(|v| v.retain(|t| t.id != id));
                    left
                }
            };
            let next = list.first().map(|t| t.id);
            threads.set(list);
            if was_open {
                current.set(None);
                msgs.set(Vec::new());
                match next {
                    Some(n) => open_thread(n),
                    None => {
                        stats.set(None);
                        sync_url(None);
                    }
                }
            }
        });
    };

    // Pin/Unpin and Archive/Restore from a row's menu, or the open archived
    // thread's Restore. The server's answer is the updated thread: applied
    // to `current` when that is the one touched, then the list is re-read —
    // a pin or an archive both move where (or whether) a thread is listed.
    let pin_thread = move |id: i64, pinned: bool| {
        scope.spawn(async move {
            match crate::api::post::<ChatThread, _>(
                format!("/chat/api/threads/{id}/pin"),
                &json!({ "pinned": pinned }),
            )
            .await
            {
                Ok(updated) => {
                    current.try_update(|c| {
                        if let Some(t) = c.as_mut().filter(|t| t.id == id) {
                            *t = updated;
                        }
                    });
                    refresh_threads();
                }
                Err(e) => toasts.err(format!(
                    "{} failed: {e}",
                    if pinned { "pin" } else { "unpin" }
                )),
            }
        });
    };
    let archive_thread = move |id: i64, archived: bool| {
        scope.spawn(async move {
            match crate::api::post::<ChatThread, _>(
                format!("/chat/api/threads/{id}/archive"),
                &json!({ "archived": archived }),
            )
            .await
            {
                Ok(updated) => {
                    current.try_update(|c| {
                        if let Some(t) = c.as_mut().filter(|t| t.id == id) {
                            *t = updated;
                        }
                    });
                    refresh_threads();
                }
                Err(e) => toasts.err(format!(
                    "{} failed: {e}",
                    if archived { "archive" } else { "restore" }
                )),
            }
        });
    };

    // A settings patch for the open thread. The thread is updated when the
    // server has taken it, not before, and the list is re-read: a settings
    // write moves the thread to the top and may change its model label and
    // what the model filter finds (code:A4).
    let patch_thread = move |body: Value| {
        let Some(t) = current.get_untracked() else {
            return;
        };
        let id = t.id;
        spawn_local(async move {
            let res =
                crate::api::post::<Value, _>(format!("/chat/api/threads/{id}/settings"), &body)
                    .await;
            match res {
                Ok(answer) => {
                    if body.get("model_alias").is_none() {
                        toasts.ok("thread settings saved");
                    }
                    // `try_`: a no-op once the page is gone.
                    current.try_update(|c| {
                        if let Some(t) = c.as_mut().filter(|t| t.id == id) {
                            apply_settings(t, &body);
                            // The new model or reasoning may change whether
                            // the last reply can be continued.
                            if let Ok(c) =
                                serde_json::from_value::<ContinueState>(answer["continue"].clone())
                            {
                                t.cont = Some(c);
                            }
                            // The voice as the server took it, and what it
                            // resolves to now.
                            super::chat_voice::apply_answer(t, &answer);
                        }
                    });
                    if scope.alive() {
                        refresh_threads();
                    }
                    // The drafts' blockers depend on the model: re-read them.
                    if body.get("model_alias").is_some() && scope.alive() {
                        refresh_drafts(draft_attachments, scope, id, move |t| {
                            current.with_untracked(|c| c.as_ref().map(|x| x.id) == Some(t))
                        });
                    }
                }
                Err(e) => toasts.err(format!("saving the thread settings failed: {e}")),
            }
        });
    };
    // A model picked for the open thread → persist immediately. Opening a
    // thread sets the picker to that thread's model too, and that is not a
    // pick: the thread is compared as well, so browsing writes nothing (a
    // settings write also bumps the thread to the top of the list).
    let current_id = Memo::new(move |_| current.with(|c| c.as_ref().map(|t| t.id)));
    let settings = SettingsDraft::new();
    Effect::new(move |prev: Option<Option<i64>>| {
        let id = current_id.get();
        if prev != Some(id) {
            settings.seed(&current.get_untracked().unwrap_or_default());
        }
        id
    });
    // A seed "New voice" drew is pending until the thread holds it.
    let held_seed = Memo::new(move |_| current.with(|c| c.as_ref().and_then(|t| t.voice.seed)));
    Effect::new(move |_| settings.voice.settle_seed(held_seed.get()));
    let settings_unsaved =
        Memo::new(move |_| current.with(|c| c.as_ref().is_some_and(|t| settings.differs_from(t))));
    use_dirty_guard().watch_page("the thread settings", settings_unsaved.into());
    Effect::new(move |prev: Option<(Option<i64>, String)>| {
        let sel = model_sel.get();
        let id = current_id.get();
        if let Some((prev_id, prev_sel)) = prev {
            if id.is_some() && prev_id == id && prev_sel != sel && !sel.is_empty() {
                patch_thread(json!({ "model_alias": sel }));
            }
        }
        (id, sel)
    });

    let ctx_max = Memo::new(move |_| {
        let sel = model_sel.get();
        catalog.entries.with(|e| {
            e.iter()
                .find(|m| m.id == sel)
                .and_then(|m| m.ctx)
                .map(|c| c as i64)
        })
    });
    // `Some(true)`/unknown both send images; only `Some(false)` gates them —
    // the same reading the gateway's own send-time rule takes (design §2).
    let model_vision = Memo::new(move |_| {
        let sel = model_sel.get();
        catalog
            .entries
            .with(|e| e.iter().find(|m| m.id == sel).and_then(|m| m.vision))
    });
    let vision_no = Signal::derive(move || model_vision.get() == Some(false));
    // The one Send gate: the server's blockers on the drafts (images, PDF
    // modes, audio), with the local vision check as the fallback for a draft
    // it has not answered for yet (chat_attach::select_blockers).
    // What the drafts become on the way, sent all the same (a GPU block's
    // fallback that cannot see): said, never blocking.
    let attach_hints = Signal::derive(move || draft_attachments.with(|v| draft_hints(v)));
    let attach_blockers = Signal::derive(move || {
        draft_attachments.with(|v| draft_blockers(v, vision_no.get(), &model_sel.get()))
    });
    let on_refresh_drafts = Callback::new(move |()| {
        if let Some(tid) = current_id.get_untracked() {
            refresh_drafts(draft_attachments, scope, tid, move |t| {
                current_id.get_untracked() == Some(t)
            });
        }
    });

    // Switching the toolbar's Archived toggle re-fetches that list — a
    // stale-list flash of the other view is worse than a beat of "Loading…".
    Effect::new(move |prev: Option<bool>| {
        let va = view_archived.get();
        if prev.is_some() && prev != Some(va) {
            threads_state.set(None);
            refresh_threads();
        }
        va
    });

    // Upload a picked/dropped/pasted file into the open thread's draft chips
    // at once — what is on screen is always what a Send would bind, never a
    // local-only file the server has not seen. The chip's signals are made
    // under the page's owner (see `owner` above), not this closure's.
    let upload_file = move |file: web_sys::File| {
        let Some(t) = current.get_untracked() else {
            toasts.err("open or create a chat first");
            return;
        };
        let tid = t.id;
        let name = file.name();
        let size = Some(file.size() as i64);
        let Some(chip) = in_owner(owner, || new_draft(alloc_key(), name.clone(), size)) else {
            return;
        };
        draft_attachments.update(|v| v.push(chip.clone()));
        scope.spawn(async move {
            let encoded = js_sys::encode_uri_component(&name)
                .as_string()
                .unwrap_or_default();
            let url = format!("/chat/api/threads/{tid}/attachments?name={encoded}");
            match crate::api::post_raw::<Attachment>(url, file).await {
                Ok(a) => chip.uploaded(a),
                Err(e) => {
                    chip.uploading.set(false);
                    // The chip keeps the message; the toast is where a long
                    // one (the accepted-types list) reads in full.
                    toasts.err(e.to_string());
                    chip.error.set(Some(e.to_string()));
                }
            }
        });
    };
    let upload_files = move |files: web_sys::FileList| {
        for i in 0..files.length() {
            if let Some(f) = files.get(i) {
                upload_file(f);
            }
        }
    };

    // ✕ on a chip: an uploaded draft is deleted server-side (fire and
    // forget — it is gone from the composer either way); an error chip has
    // nothing on the server to delete.
    let remove_draft = move |chip: DraftChip| {
        draft_attachments.update(|v| v.retain(|c| c.key != chip.key));
        if let Some(id) = chip.id.get_untracked() {
            spawn_local(async move {
                if let Err(e) = crate::api::post::<Value, _>(
                    format!("/chat/api/attachments/{id}/delete"),
                    &json!({}),
                )
                .await
                {
                    toasts.err(format!("removing the attachment failed: {e}"));
                }
            });
        }
    };

    // A sent chip's own click: an image opens full-size, a text file's
    // content is fetched (it is served as plain bytes, not JSON) and shown
    // in a `<pre>`.
    let open_attachment = Callback::new(move |a: Attachment| {
        let gen = viewer_gen.get_value() + 1;
        viewer_gen.set_value(gen);
        viewer_name.set(a.name.clone());
        viewer_is_image.set(a.kind == "image");
        viewer_att.set(Some(a.clone()));
        viewer_open.set(true);
        if a.kind == "image" {
            viewer_src.set(format!("/chat/api/attachments/{}", a.id));
            return;
        }
        viewer_src.set(String::new());
        viewer_loading.set(true);
        scope.spawn(async move {
            let text =
                match crate::api::get_text(format!("/chat/api/attachments/{}/text", a.id)).await {
                    Ok(t) => t,
                    Err(e) => format!("failed to load: {e}"),
                };
            // A later open (of this attachment or another) may have already
            // moved the viewer on — only the fetch that is still current
            // gets to write `viewer_src`.
            if viewer_gen.get_value() == gen {
                viewer_src.set(text);
                viewer_loading.set(false);
            }
        });
    });

    // A code block's Preview button (assets/codeblocks.js) fires a window
    // CustomEvent; the modal it opens is ours. The listener lives outside the
    // reactive tree, so it only touches these signals — never context.
    let preview_open = RwSignal::new(false);
    let preview_code = RwSignal::new(String::new());
    let preview_lang = RwSignal::new(String::new());
    let listener = window_event_listener_untyped("lmgw-preview", move |ev| {
        let Ok(detail) = js_sys::Reflect::get(&ev, &JsValue::from_str("detail")) else {
            return;
        };
        let field = |k: &str| {
            js_sys::Reflect::get(&detail, &JsValue::from_str(k))
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_default()
        };
        preview_code.set(field("code"));
        preview_lang.set(field("lang"));
        preview_open.set(true);
    });
    on_cleanup(move || listener.remove());

    // Every streamed turn — send, regenerate, edit, continue — reads and
    // writes the page through this (chat_turn.rs); the message actions
    // (chat_actions.rs) are built on it.
    let turn_env = TurnEnv {
        msgs,
        current,
        current_id,
        streaming,
        live,
        stats,
        aborter,
        ctx_max,
        toasts,
        scope,
        voice: page_voice,
    };
    let actions = ActionEnv {
        turn: turn_env,
        owner,
        next_key,
        refresh: Callback::new(move |()| refresh_threads()),
    };
    let msg_ops = MsgOps {
        msgs,
        busy: Signal::derive(move || streaming.get().is_some()),
        cont: Signal::derive(move || current.with(|c| c.as_ref().and_then(|t| t.cont.clone()))),
        regenerate: Callback::new(move |key| actions.regenerate(key)),
        edit: Callback::new(move |r: EditReq| {
            let is_user = msgs.with_untracked(|v| {
                v.iter()
                    .find(|m| m.key == r.key)
                    .is_some_and(|m| m.role == "user")
            });
            if is_user {
                actions.edit_user(r.key, r.text, r.kb_refs, r.done);
            } else {
                actions.edit_assistant(r.key, r.text, r.done);
            }
        }),
        resume: Callback::new(move |()| actions.resume()),
        delete: Callback::new(move |key| actions.delete(key)),
    };

    // Leaving the Chat page discards the open temporary chat too.
    {
        use std::sync::atomic::Ordering::Relaxed;
        let track = open_temp.clone();
        Effect::new(move |_| {
            let id = current
                .with(|c| c.as_ref().filter(|t| t.temporary).map(|t| t.id))
                .unwrap_or(0);
            track.store(id, Relaxed);
        });
        on_cleanup(move || {
            let id = open_temp.load(Relaxed);
            if id < 0 {
                if let Some(a) = aborter.try_get_value().flatten() {
                    a.abort();
                }
                chat_temp::discard(id);
            }
        });
    }

    let stop = move |_| {
        if let Some(a) = aborter.get_value() {
            a.abort();
        }
    };

    let send = move |_| {
        if let Some(busy) = streaming.get_untracked() {
            if current_id.get_untracked() != Some(busy) {
                toasts.warn("a reply is still streaming in another conversation");
            }
            return;
        }
        // A dictation is finished or discarded first (chat-voice §5).
        if !page_voice.dictation.before_send() {
            return;
        }
        let text = composer.get_untracked().trim().to_string();
        if draft_attachments.with_untracked(|v| v.iter().any(|c| c.uploading.get_untracked())) {
            toasts.warn("attachments are still uploading");
            return;
        }
        let ready: Vec<DraftChip> = draft_attachments
            .get_untracked()
            .into_iter()
            .filter(|c| c.id.get_untracked().is_some() && c.error.get_untracked().is_none())
            .collect();
        if text.is_empty() && ready.is_empty() {
            return;
        }
        if let Some(why) = attach_blockers.get_untracked().first() {
            // Send is disabled for this already; Enter bypasses that button,
            // so it must say why nothing happened instead of doing nothing
            // (review nit).
            toasts.warn(why.clone());
            return;
        }
        let Some(t) = current.get_untracked() else {
            toasts.err("open or create a chat first");
            return;
        };
        let ids: Vec<i64> = ready.iter().filter_map(|c| c.id.get_untracked()).collect();
        let att_meta: Vec<Attachment> = ready.iter().map(DraftChip::as_attachment).collect();
        // Kept to restore the composer if the send is refused outright
        // (review finding: a refusal used to silently lose the typed text
        // and the draft chips).
        let restore_text = text.clone();
        let restore_chips = draft_attachments.get_untracked();
        let kb_refs = draft_kbs.get_untracked();
        let restore_kbs = kb_refs.clone();
        // Dictated text goes as a spoken turn (chat-voice §5).
        let spoken = page_voice.dictation.take_mark();
        composer.set(String::new());
        draft_attachments.set(Vec::new());
        draft_kbs.set(Vec::new());
        let Some((user, assistant)) = in_owner(owner, || {
            let u = new_msg(alloc_key(), "user", text.clone());
            u.attachments.set(att_meta);
            u.kb_refs.set(kb_refs.clone());
            u.voice.set(spoken.as_ref().map(|m| m.as_msg_voice()));
            (u, new_msg(alloc_key(), "assistant", String::new()))
        }) else {
            return;
        };
        let user_key = user.key;
        let assistant_key = assistant.key;
        let tid = t.id;
        let user_db = user.db_id;
        let reply = assistant.clone();
        msgs.update(|m| {
            m.push(user);
            m.push(assistant);
        });
        scroll_down(true);
        let mut body = if kb_refs.is_empty() {
            json!({ "content": text, "attachments": ids })
        } else {
            json!({ "content": text, "attachments": ids, "kb_refs": kb_refs })
        };
        page_voice
            .dictation
            .voice_for_send(spoken.as_ref(), &mut body);
        let turn = Turn {
            tid,
            url: format!("/chat/api/threads/{tid}/send"),
            body,
            target: reply,
            continuing: false,
            what: "send",
        };
        spawn_local(async move {
            let end = run_turn(turn_env, turn, move |id| user_db.set(Some(id)), || {}).await;
            if end.refused {
                // Refused before any SSE frame arrived: nothing went
                // anywhere, so give back exactly what Send took — the
                // typed text, the chips, and the optimistic bubbles it
                // had provisionally drawn (review finding: a refusal
                // silently lost all three, and left empty user/assistant
                // bubbles in the transcript). A failure once the stream
                // had already started keeps today's behaviour.
                composer.set(restore_text.clone());
                draft_attachments.set(restore_chips.clone());
                draft_kbs.set(restore_kbs.clone());
                page_voice.dictation.restore_mark(spoken.clone());
                msgs.update(|m| m.retain(|msg| msg.key != user_key && msg.key != assistant_key));
            }
            refresh_threads();
            if scope.alive() {
                actions.resync(tid, false);
            }
        });
    };

    // Stop and Send follow the thread on screen: rebuilt when that flips,
    // not with every delta.
    let streaming_here = Memo::new(move |_| {
        let s = streaming.get();
        s.is_some() && s == current_id.get()
    });
    let streaming_elsewhere = Memo::new(move |_| {
        let s = streaming.get();
        s.is_some() && s != current_id.get()
    });
    // Voice mode (chat-voice WP9): the realtime panel in the composer's
    // place. It reads the thread back when a session ends (`load`).
    let realtime = chat_voice::provide_realtime(
        page_voice,
        chat_voice::RealtimeParts {
            msgs,
            current,
            owner,
            next_key,
            scope,
            streaming,
            model_sel,
            draft: settings.voice,
            refresh: Callback::new(move |()| refresh_threads()),
            load: Callback::new(load_msgs),
            make: Callback::new(msgs_of_rows),
        },
    );
    let voice_mode = page_voice.voice_mode;
    let is_temporary =
        Memo::new(move |_| current.with(|c| c.as_ref().is_some_and(|t| t.temporary)));
    let is_admin =
        Memo::new(move |_| current.with(|c| c.as_ref().is_some_and(|t| t.kind == "admin")));
    let mcp_names = Memo::new(move |_| {
        current.with(|c| {
            c.as_ref()
                .map(|t| {
                    t.mcp_tools
                        .iter()
                        .map(|m| m.server_label.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    });
    let folders = FolderEnv::new(
        folder_list,
        scope,
        model_sel,
        current,
        Callback::new(open_thread),
        Callback::new(move |()| refresh_threads()),
        Callback::new(delete_thread),
    );
    let query = RwSignal::new(String::new());
    let listed = Memo::new(move |_| {
        let today = today_local();
        let q = query.get();
        let archived = view_archived.get();
        // The temporary group leads the active view only.
        let mut out = if archived {
            Vec::new()
        } else {
            temp_threads
                .with(|t| chat_temp::temp_items(t, &crate::widgets::filter_words(&q), &today))
        };
        let collapsed = folders.collapsed();
        out.extend(
            folder_list.with(|f| thread_items(&threads.get(), f, &collapsed, &q, &today, archived)),
        );
        out
    });
    let badge = Signal::derive(move || {
        let shown = listed.with(|l| {
            l.iter()
                .filter(|i| matches!(i, ListItem::Row { .. }))
                .count()
        });
        let temps = if view_archived.get() {
            0
        } else {
            temp_threads.with(Vec::len)
        };
        crate::fmt::of(shown, threads.with(Vec::len) + temps)
    });
    let on_new = Callback::new(new_thread);
    let on_open = Callback::new(open_thread);
    let on_pick = Callback::new(move |(tid, mid): (i64, Option<i64>)| {
        focus.set(mid);
        open_thread(tid);
        // Already open: `open_thread` returned at once, so settle here.
        if current.with_untracked(|c| c.as_ref().map(|t| t.id) == Some(tid)) {
            settle_focus();
        }
    });
    let on_delete = Callback::new(delete_thread);
    let on_pin = Callback::new(move |(id, pinned): (i64, bool)| pin_thread(id, pinned));
    let on_archive = Callback::new(move |(id, archived): (i64, bool)| archive_thread(id, archived));

    // Whether attachments can go out at all: any still uploading blocks a
    // send outright, and so does any blocker (`attach_blockers`, design §3/§8).
    let uploading_now =
        Signal::derive(move || draft_attachments.with(|v| v.iter().any(|c| c.uploading.get())));
    let can_send = Signal::derive(move || {
        let ready = draft_attachments.with(|v| {
            v.iter()
                .any(|c| c.id.get().is_some() && c.error.get().is_none())
        });
        (!composer.get().trim().is_empty() || ready)
            && !uploading_now.get()
            && attach_blockers.with(Vec::is_empty)
    });
    let send_disabled_reason = Signal::derive(move || {
        if streaming_elsewhere.get() {
            "a reply is still streaming in another conversation — wait for it, or stop it there"
                .to_string()
        } else if let Some(why) = attach_blockers.get().first() {
            why.clone()
        } else if uploading_now.get() {
            "attachments are still uploading".to_string()
        } else {
            String::new()
        }
    });

    // The open thread's archive strip, when it is archived: "Archived <date>
    // · deleted <relative>" plus Restore.
    let archive_info = Signal::derive(move || {
        current.with(|c| {
            c.as_ref().and_then(|t| {
                t.archived_at.as_ref().map(|a| {
                    let when = crate::fmt::log_time(a).day_label;
                    let purge = t
                        .purge_at
                        .as_deref()
                        .map(purge_text)
                        .unwrap_or_else(|| "kept indefinitely".to_string());
                    (when, purge)
                })
            })
        })
    });

    view! {
        <div class="chat-shell">
            <SplitPane
                side=Side::Left
                persist="chat.threads"
                label="Conversations"
                badge=badge
                side_view=move || {
                    view! {
                        <ThreadList
                            items=listed
                            state=threads_state
                            retry=retry_threads
                            total=Signal::derive(move || {
                                threads.with(Vec::len) + temp_threads.with(Vec::len)
                            })
                            query=query
                            current=current_id
                            on_open=on_open
                            on_new=on_new
                            on_delete=on_delete
                            on_pin=on_pin
                            on_archive=on_archive
                            view_archived=view_archived
                            archived_count=archived_count
                            folders=folders
                            on_pick=on_pick
                        />
                    }
                }
            >
                <Show
                    when=move || current_id.get().is_some()
                    fallback=move || {
                        view! {
                            <div class="chat-empty">
                                <div class="empty">
                                    "Pick a conversation or start a new one."
                                    <button class="btn primary" on:click=move |_| new_thread("chat")>
                                        "New chat"
                                    </button>
                                </div>
                            </div>
                        }
                    }
                >
                    // Collapsed by default: the settings take the transcript's
                    // width, never its height, so the composer stays the last
                    // row whatever the panel holds.
                    <SplitPane
                        side=Side::Right
                        persist="chat.settings"
                        label="Thread settings"
                        default_open=false
                        width=(280, 30, 420)
                        auto_collapse_below=480
                        side_view=move || {
                            // One form per thread: switching threads starts
                            // from that thread's own settings.
                            move || {
                                current_id
                                    .get()
                                    .map(|_| {
                                        view! {
                                            <ThreadSettings
                                                thread=current
                                                draft=settings
                                                unsaved=settings_unsaved
                                                on_save=patch_thread
                                                is_admin=is_admin.get_untracked()
                                            />
                                        }
                                    })
                            }
                        }
                    >
                        <div
                            class="chat-main density-airy"
                            class:drag-over=move || drag_over.get()
                            on:dragover=move |ev| {
                                // Only claim the drop when it is files: text
                                // (a selection, a link) must fall through to
                                // the browser's normal drop-into-a-field
                                // handling instead of being swallowed.
                                if ev.data_transfer().is_some_and(|dt| drag_has_files(&dt)) {
                                    ev.prevent_default();
                                    drag_over.set(true);
                                }
                            }
                            on:dragleave=move |ev| {
                                ev.prevent_default();
                                drag_over.set(false);
                            }
                            on:drop=move |ev| {
                                let Some(dt) = ev.data_transfer() else { return };
                                if !drag_has_files(&dt) {
                                    return;
                                }
                                ev.prevent_default();
                                drag_over.set(false);
                                if let Some(files) = dt.files() {
                                    upload_files(files);
                                }
                            }
                        >
                            <div class="chat-head">
                                <ModelPicker value=model_sel tasks=&["chat"] recent_key="chat"/>
                                <Show when=move || is_admin.get()>
                                    <span class="type-badge" title="self-admin tools attached">
                                        "admin"
                                    </span>
                                </Show>
                                {move || {
                                    current
                                        .with(|c| c.as_ref().and_then(reasoning_badge))
                                        .map(|b| {
                                            view! {
                                                <span
                                                    class="type-badge"
                                                    title="This thread's reasoning overrides — Thread settings → Reasoning"
                                                >
                                                    {b}
                                                </span>
                                            }
                                        })
                                }}
                                {move || {
                                    let names = mcp_names.get();
                                    (!names.is_empty())
                                        .then(|| {
                                            view! {
                                                <span
                                                    class="type-badge"
                                                    title=format!(
                                                        "MCP servers attached: {}",
                                                        names.join(", "),
                                                    )
                                                >
                                                    {format!("{} mcp", names.len())}
                                                </span>
                                            }
                                        })
                                }}
                                {move || {
                                    current
                                        .with(|c| c.as_ref().and_then(kb_badge))
                                        .map(|b| {
                                            view! {
                                                <span
                                                    class="type-badge"
                                                    title="Knowledge bases on this thread — Thread settings → Knowledge"
                                                >
                                                    {b}
                                                </span>
                                            }
                                        })
                                }}
                                <Show when=move || current_id.get().is_some()>
                                    <super::chat_export::ExportMenu thread=current_id/>
                                </Show>
                            </div>
                            <div class="chat-scroll" id="chat-scroll">
                                <div class="chat-col">
                                    <For each=move || msgs.get() key=|m| m.key let:m>
                                        <MsgView m=m vision_no=vision_no on_open=open_attachment ops=msg_ops/>
                                    </For>
                                </div>
                            </div>
                            <StatsRow stats=Signal::derive(move || {
                                let here = current_id.get();
                                stats
                                    .get()
                                    .filter(|(t, _)| Some(*t) == here)
                                    .map(|(_, s)| s)
                            })/>
                            <Show when=move || is_temporary.get()>
                                <TempBanner
                                    thread=current_id
                                    streaming=streaming_here
                                    voice_mode=voice_mode
                                    voice_closing=realtime.closing
                                    busy=keep_busy
                                    on_keep=Callback::new(move |()| {
                                        if let Some(id) = current_id.get_untracked() {
                                            chat_temp::keep(
                                                id,
                                                current,
                                                keep_busy,
                                                scope,
                                                toasts,
                                                on_open,
                                                Callback::new(move |()| refresh_threads()),
                                            );
                                        }
                                    })
                                />
                            </Show>
                            {move || {
                                archive_info
                                    .get()
                                    .map(|(when, purge)| {
                                        view! {
                                            <div class="archive-strip">
                                                <span>{format!("Archived {when} · {purge}")}</span>
                                                <button
                                                    class="btn ghost sm"
                                                    on:click=move |_| {
                                                        if let Some(id) = current_id.get_untracked() {
                                                            archive_thread(id, false);
                                                        }
                                                    }
                                                >
                                                    "Restore"
                                                </button>
                                            </div>
                                        }
                                    })
                            }}
                            // The composer and what belongs to it, hidden (never
                            // dropped: its text, chips and dictation mark wait)
                            // while voice mode has its place.
                            <div class="composer-area" style:display=move || if voice_mode.get() { "none" } else { "contents" }>
                            <DraftChips
                                chips=draft_attachments
                                vision_no=vision_no
                                on_remove=Callback::new(remove_draft)
                                on_open=open_attachment
                                on_refresh=on_refresh_drafts
                            />
                            <BlockerNote blockers=attach_blockers/>
                            <HintNote hints=attach_hints/>
                            <KbDraftChips chips=draft_kbs/>
                            <chat_voice::VoiceStatusLine pv=page_voice/>
                            <div class="composer-wrap">
                            <div class="composer" node_ref=composer_box>
                                <KbPopover pick=kb_pick anchor=composer_box/>
                                <input
                                    type="file"
                                    multiple
                                    style="display:none"
                                    node_ref=file_input
                                    on:change=move |ev| {
                                        let el: web_sys::HtmlInputElement = event_target(&ev);
                                        if let Some(files) = el.files() {
                                            upload_files(files);
                                        }
                                        el.set_value("");
                                    }
                                />
                                <div class="composer-tools">
                                <button
                                    type="button"
                                    class="btn ghost composer-attach"
                                    title="Attach files"
                                    on:click=move |_| {
                                        if let Some(el) = file_input.get_untracked() {
                                            el.click();
                                        }
                                    }
                                >
                                    // Monochrome line icon, like every other
                                    // icon in the app — the paperclip emoji
                                    // rendered in colour otherwise (review nit).
                                    <svg viewBox="0 0 16 16">
                                        <path d="M14.29 7.37l-6.13 6.13a4 4 0 0 1-5.66-5.66l6.13-6.13a2.67 2.67 0 0 1 3.77 3.77l-6.13 6.13a1.33 1.33 0 0 1-1.89-1.89l5.65-5.65"/>
                                    </svg>
                                </button>
                                <KbButton pick=kb_pick/>
                                </div>
                                <textarea
                                    class="input ta composer-input"
                                    node_ref=composer_ta
                                    title=chat_voice::composer_title(Some(page_voice))
                                    placeholder="Message… (Enter sends, Shift+Enter for a newline, # adds knowledge)"
                                    prop:value=move || composer.get()
                                    on:input=move |ev| kb_pick.on_input(&ev)
                                    on:click=move |ev| kb_pick.on_caret(&ev)
                                    on:keyup=move |ev| kb_pick.on_caret(&ev)
                                    on:keydown=move |ev| {
                                        if kb_pick.on_key(&ev) {
                                            return;
                                        }
                                        if ev.key() == "Enter" && !ev.shift_key() {
                                            ev.prevent_default();
                                            send(());
                                        }
                                    }
                                    on:paste=move |ev| {
                                        if let Some(files) = ev.clipboard_data().and_then(|dt| dt.files()) {
                                            if files.length() > 0 {
                                                ev.prevent_default();
                                                upload_files(files);
                                            }
                                        }
                                    }
                                ></textarea>
                                <chat_voice::VoiceControls
                                    draft=settings.voice
                                    on_saved=Callback::new(move |()| refresh_threads())
                                />
                                {move || {
                                    if streaming_here.get() {
                                        view! {
                                            <button class="btn danger" on:click=stop>
                                                "Stop"
                                            </button>
                                        }
                                            .into_any()
                                    } else {
                                        view! {
                                            <button
                                                class="btn primary"
                                                disabled=move || streaming_elsewhere.get() || !can_send.get()
                                                title=move || send_disabled_reason.get()
                                                on:click=move |_| send(())
                                            >
                                                "Send"
                                            </button>
                                        }
                                            .into_any()
                                    }
                                }}
                            </div>
                            </div>
                            </div>
                            <Show when=move || voice_mode.get()>
                                <chat_voice::RealtimePanel/>
                            </Show>
                        </div>
                    </SplitPane>
                </Show>
            </SplitPane>

            // HTML/SVG preview — the fenced block rendered in a sandboxed
            // frame, closed by Esc, the backdrop, or ✕ (the Modal widget).
            <Modal open=preview_open title="Preview" size=ModalSize::Full fill=true>
                <span class="preview-lang">
                    {move || {
                        let l = preview_lang.get();
                        if l.is_empty() { "html".to_string() } else { l }
                    }}
                </span>
                // Mounted only while the modal is open: an <iframe> created
                // inside a closed <dialog> never gets a browsing context, and
                // writing srcdoc later does not revive it (Chromium/WebKit) —
                // it would render a permanently blank frame.
                <Show when=move || preview_open.get()>
                    <iframe
                        class="preview-frame"
                        sandbox="allow-scripts allow-modals allow-forms allow-popups allow-pointer-lock"
                        srcdoc=move || preview_code.get()
                    ></iframe>
                </Show>
            </Modal>

            // A sent chip's own click: an image full-size, or a text file's
            // fetched content in a `<pre>`.
            <FolderDialogs env=folders/>
            <SourceModal target=kbui.source/>
            // `fill`: the body holds one `.fill-pane`, the only scroller (a
            // `<pre>` with its own max-height inside a scrolling body was two).
            <Modal open=viewer_open title="Attachment" size=ModalSize::Wide fill=true>
                <div class="dim mono-sm" style="margin-bottom:8px">{move || viewer_name.get()}</div>
                <ViewerMeta att=viewer_att/>
                {move || {
                    if viewer_is_image.get() {
                        view! {
                            <div class="fill-pane">
                                <img class="attach-modal-img" src=move || viewer_src.get()/>
                            </div>
                        }
                            .into_any()
                    } else if viewer_loading.get() {
                        view! { <div class="dim">"Loading…"</div> }.into_any()
                    } else {
                        view! { <pre class="preset attach-text fill-pane">{move || viewer_src.get()}</pre> }.into_any()
                    }
                }}
            </Modal>
        </div>
    }
}

// ---------------------------------------------------------------------------
// Thread list
// ---------------------------------------------------------------------------

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// Days since 1970-01-01 of a `YYYY-MM-DD` day (proleptic Gregorian — Howard
/// Hinnant's `days_from_civil`), so "yesterday" and "this week" are a
/// subtraction rather than a date library.
fn day_number(day: &str) -> Option<i64> {
    let y: i64 = day.get(0..4)?.parse().ok()?;
    let m: i64 = day.get(5..7)?.parse().ok()?;
    let d: i64 = day.get(8..10)?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

/// The heading a thread files under: its local day against today's.
fn day_group(day: &str, today: &str) -> String {
    let (Some(d), Some(t)) = (day_number(day), day_number(today)) else {
        return "Undated".to_string();
    };
    match t - d {
        // A clock a little ahead of the server's is still today.
        n if n <= 0 => "Today".to_string(),
        1 => "Yesterday".to_string(),
        n if n < 7 => "Previous 7 days".to_string(),
        _ => {
            let m: usize = day[5..7].parse().unwrap_or(1);
            format!("{} {}", MONTHS[(m - 1).min(11)], &day[0..4])
        }
    }
}

/// Today in the reader's own time, `YYYY-MM-DD`.
fn today_local() -> String {
    let d = js_sys::Date::new_0();
    format!(
        "{:04}-{:02}-{:02}",
        d.get_full_year(),
        d.get_month() + 1,
        d.get_date()
    )
}

/// A stored `YYYY-MM-DD HH:MM:SS` (SQLite `datetime('now')`, UTC) as seconds
/// since the epoch — the same parsing [`crate::fmt::log_time`] does, needed
/// here as a number rather than a rendered string so [`crate::fmt::rel_time_at`]
/// can turn a future `purge_at` into "in 12d".
fn parse_utc_ts(ts: &str) -> Option<f64> {
    let zoned = ts.ends_with('Z') || ts.get(10..).is_some_and(|t| t.contains('+'));
    let iso = if zoned {
        ts.replacen(' ', "T", 1)
    } else {
        format!("{}Z", ts.get(0..19).unwrap_or(ts).replacen(' ', "T", 1))
    };
    let d = js_sys::Date::new(&JsValue::from_str(&iso));
    let t = d.get_time();
    (!t.is_nan()).then_some(t / 1000.0)
}

/// What an archived row says about its fate: "deleted in 12d" from
/// `purge_at`, verbatim when it will not parse rather than nothing. A
/// `purge_at` already in the past means the hourly sweep just has not run
/// yet (review nit: this used to read the confusing "deleted 2h ago" for a
/// thread that has not actually been deleted).
fn purge_text(purge_at: &str) -> String {
    let Some(epoch) = parse_utc_ts(purge_at) else {
        return format!("deleted {purge_at}");
    };
    let now = js_sys::Date::now() / 1000.0;
    if epoch <= now {
        return "deleted at the next sweep".to_string();
    }
    format!("deleted {}", crate::fmt::rel_time_at(epoch, now))
}

/// One line of the thread list: a date heading with its count, or a thread.
#[derive(Clone, PartialEq)]
// Short-lived render rows; boxing `Row` would ripple through every match arm for no gain.
#[allow(clippy::large_enum_variant)]
pub(super) enum ListItem {
    Head(String, usize),
    /// A folder's header (chat-complete §5); its threads follow as nested rows.
    Folder(FolderRow),
    Row {
        thread: ChatThread,
        /// Listed under its folder's header, so indented.
        nested: bool,
        /// The folder's name, shown in the flat archived view.
        folder: Option<String>,
        /// "14:32" under Today and Yesterday, "Thu 24 Sep" further back.
        when: String,
        /// The local date and time, and what was stored (UTC).
        when_title: String,
    },
}

impl ListItem {
    /// This row, listed under its folder.
    pub(super) fn nested(mut self) -> Self {
        if let ListItem::Row { nested, .. } = &mut self {
            *nested = true;
        }
        self
    }

    /// This row, naming its folder.
    fn in_folder(mut self, name: Option<&str>) -> Self {
        if let ListItem::Row { folder, .. } = &mut self {
            *folder = name.map(str::to_string);
        }
        self
    }

    /// Identity plus everything it shows: a keyed list never updates an
    /// entry in place — pin and archive state included, so toggling either
    /// swaps the row instead of leaving its mark stale.
    fn key(&self) -> String {
        match self {
            ListItem::Head(label, n) => format!("h:{label}:{n}"),
            ListItem::Folder(f) => f.key(),
            ListItem::Row {
                thread: t,
                when,
                nested,
                folder,
                ..
            } => format!(
                "t:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
                t.id,
                t.title,
                t.model_alias,
                t.kind,
                t.agent_id.as_deref().unwrap_or(""),
                when,
                t.pinned,
                t.archived_at.as_deref().unwrap_or(""),
                t.purge_at.as_deref().unwrap_or(""),
                nested,
                folder.as_deref().unwrap_or(""),
            ),
        }
    }
}

/// Every word of `query` in the title or the model — shared by the pinned,
/// dated and archived buckets below.
pub(super) fn matches_query(t: &ChatThread, words: &[String]) -> bool {
    matches_query_in(t, None, words)
}

/// [`matches_query`], with the name of the thread's folder counting as part
/// of its text: filtering by a folder's name finds what is inside it.
pub(super) fn matches_query_in(t: &ChatThread, folder: Option<&str>, words: &[String]) -> bool {
    let hay = format!(
        "{} {} {}",
        t.title,
        t.model_alias,
        folder.unwrap_or_default()
    )
    .to_lowercase();
    words.iter().all(|w| hay.contains(w.as_str()))
}

/// A thread as a row: "14:32" under Today and Yesterday, "Thu 24 Sep"
/// further back — its last-active time either way, pinned or not.
pub(super) fn thread_row(t: &ChatThread, today: &str) -> ListItem {
    let lt = crate::fmt::log_time(&t.updated_at);
    let group = day_group(&lt.day, today);
    let recent = group == "Today" || group == "Yesterday";
    ListItem::Row {
        when: if recent {
            lt.time.get(0..5).unwrap_or(&lt.time).to_string()
        } else {
            lt.day_label.clone()
        },
        when_title: format!("{} {} · {}", lt.day, lt.time, lt.utc),
        thread: t.clone(),
        nested: false,
        folder: None,
    }
}

/// The threads that match `query` (every word, over title, model and folder
/// name).
///
/// The active view: the Folders block first (each folder with its threads,
/// collapsed ones as a header only — [`chat_folders::folder_block`]); then a
/// "Pinned" group of the pinned threads *without* a folder (pinning never
/// touches `updated_at`, so pinned rows keep the server's own order rather
/// than being re-bucketed by day); then the rest under date headings, newest
/// first, as the server lists them. A thread is listed exactly once: one in a
/// folder is not repeated under Pinned or a date.
///
/// The archived view (`archived`) is flat — no date headings, no folder
/// blocks, each row naming its folder. Archived rows are ordered by
/// `archived_at`, which does not correlate with `updated_at` (the day a row's
/// last message landed), so bucketing them by day would scatter the same day
/// across the list instead of grouping it.
pub(super) fn thread_items(
    threads: &[ChatThread],
    folders: &[FolderInfo],
    collapsed: &std::collections::HashSet<i64>,
    query: &str,
    today: &str,
    archived: bool,
) -> Vec<ListItem> {
    group_threads(
        threads,
        folders,
        collapsed,
        query,
        archived,
        &|t| thread_row(t, today),
        &|t| day_group(&crate::fmt::log_time(&t.updated_at).day, today),
    )
}

/// [`thread_items`] with the two things that read the clock — a thread's row
/// and its date heading — passed in, so the grouping itself is plain logic.
pub(super) fn group_threads(
    threads: &[ChatThread],
    folders: &[FolderInfo],
    collapsed: &std::collections::HashSet<i64>,
    query: &str,
    archived: bool,
    row: &dyn Fn(&ChatThread) -> ListItem,
    date_group: &dyn Fn(&ChatThread) -> String,
) -> Vec<ListItem> {
    let words = crate::widgets::filter_words(query);
    let mut out: Vec<ListItem> = Vec::new();
    let name_of = |t: &ChatThread| chat_folders::folder_name(t, folders);

    if archived {
        out.extend(
            threads
                .iter()
                .filter(|t| matches_query_in(t, name_of(t), &words))
                .map(|t| row(t).in_folder(name_of(t))),
        );
        return out;
    }

    out.extend(chat_folders::folder_block(
        threads, folders, collapsed, &words, row,
    ));
    // What the folder block did not list.
    let unfiled = |t: &&ChatThread| name_of(t).is_none();

    let pinned: Vec<&ChatThread> = threads
        .iter()
        .filter(|t| t.pinned && matches_query(t, &words))
        .filter(unfiled)
        .collect();
    if !pinned.is_empty() {
        out.push(ListItem::Head("Pinned".to_string(), pinned.len()));
        out.extend(pinned.into_iter().map(row));
    }

    let mut head_at: Option<usize> = None;
    for t in threads.iter().filter(|t| !t.pinned).filter(unfiled) {
        if !matches_query(t, &words) {
            continue;
        }
        let group = date_group(t);
        let open_new = match head_at.and_then(|i| out.get(i)) {
            Some(ListItem::Head(label, _)) => *label != group,
            _ => true,
        };
        if open_new {
            head_at = Some(out.len());
            out.push(ListItem::Head(group, 0));
        }
        if let Some(ListItem::Head(_, n)) = head_at.and_then(|i| out.get_mut(i)) {
            *n += 1;
        }
        out.push(row(t));
    }
    out
}

/// The conversations, filterable, under date headings, each with when it was
/// last active and the model it talks to.
#[component]
fn ThreadList(
    items: Memo<Vec<ListItem>>,
    /// The list's read: loading, failed (with why), or in.
    #[prop(into)]
    state: Signal<Option<Result<(), String>>>,
    retry: Callback<()>,
    #[prop(into)] total: Signal<usize>,
    query: RwSignal<String>,
    current: Memo<Option<i64>>,
    on_open: Callback<i64>,
    on_new: Callback<&'static str>,
    on_delete: Callback<i64>,
    on_pin: Callback<(i64, bool)>,
    on_archive: Callback<(i64, bool)>,
    view_archived: RwSignal<bool>,
    #[prop(into)] archived_count: Signal<i64>,
    folders: FolderEnv,
    on_pick: Callback<(i64, Option<i64>)>,
) -> impl IntoView {
    let filtering = move || !query.with(|q| q.trim().is_empty());
    let q_box: NodeRef<leptos::html::Input> = NodeRef::new();
    use_slash_focus(q_box);
    view! {
        <div class="density-dense">
            <div class="thread-tools">
                <div class="row">
                    <button class="btn" on:click=move |_| on_new.run("chat")>
                        "New chat"
                    </button>
                    <button
                        class="btn ghost"
                        title="New Admin Chat — wired to the lmgw self-admin tools"
                        on:click=move |_| on_new.run("admin")
                    >
                        "Admin chat"
                    </button>
                    <super::chat_export::ExportAll/>
                </div>
                <button
                    type="button"
                    class="btn ghost sm"
                    title="New temporary chat — kept in memory only, discarded when you leave it"
                    on:click=move |_| on_new.run("temporary")
                >
                    "New temporary chat"
                </button>
                <button
                    type="button"
                    class="btn ghost sm"
                    title="New folder — group conversations, with settings new chats in it start from"
                    on:click=move |_| folders.create()
                >
                    "New folder"
                </button>
                <button
                    type="button"
                    class="btn ghost sm"
                    class:active=move || view_archived.get()
                    title=move || {
                        if view_archived.get() {
                            "Back to conversations"
                        } else {
                            "Threads the auto-archive sweep (or a manual Archive) filed away"
                        }
                    }
                    on:click=move |_| view_archived.update(|v| *v = !*v)
                >
                    {move || {
                        if view_archived.get() {
                            "Active conversations".to_string()
                        } else {
                            format!("Archived ({})", archived_count.get())
                        }
                    }}
                </button>
                <input
                    class="input thread-q"
                    type="search"
                    node_ref=q_box
                    data-slash
                    placeholder="Filter by title or model"
                    title="Filter · / focuses, Esc clears"
                    autocomplete="off"
                    prop:value=move || query.get()
                    on:input=move |ev| query.set(event_target_value(&ev))
                    on:keydown=move |ev| {
                        if ev.key() == "Escape" && filtering() {
                            ev.stop_propagation();
                            query.set(String::new());
                        }
                    }
                />
            </div>
            <div class="thread-list">
                <Show when=move || filtering() && items.with(Vec::is_empty)>
                    <div class="empty">
                        {move || format!("None of {} conversations match.", total.get())}
                        " "
                        <button class="link-btn" on:click=move |_| query.set(String::new())>
                            "Clear"
                        </button>
                    </div>
                </Show>
                {move || match state.get() {
                    None => view! { <div class="empty">"Loading…"</div> }.into_any(),
                    Some(Err(e)) => {
                        view! {
                            <div class="empty status-err">
                                "Could not read the conversations: " {e} " "
                                <button class="link-btn" on:click=move |_| retry.run(())>
                                    "Retry"
                                </button>
                            </div>
                        }
                            .into_any()
                    }
                    Some(Ok(())) => ().into_any(),
                }}
                <Show when=move || {
                    !filtering() && total.get() == 0 && matches!(state.get(), Some(Ok(())))
                }>
                    <div class="empty">"No conversations yet."</div>
                </Show>
                <For each=move || items.get() key=ListItem::key let:item>
                    {match item {
                        ListItem::Head(label, n) => {
                            view! {
                                <div class="thread-group">
                                    {label}
                                    <span class="count">{n}</span>
                                </div>
                            }
                                .into_any()
                        }
                        ListItem::Folder(row) => {
                            view! { <FolderHeader row=row env=folders/> }.into_any()
                        }
                        ListItem::Row { thread: t, when, when_title, nested, folder } => {
                            let id = t.id;
                            let is_open = move || current.get() == Some(id);
                            // The name, not the upstream's prefix: the row
                            // is narrow, and the full id is in the tooltip.
                            let name = t.model_alias.rsplit('/').next().unwrap_or_default();
                            let model = if name.is_empty() {
                                "no model".to_string()
                            } else {
                                name.to_string()
                            };
                            let model_title = t.model_alias.clone();
                            let pinned = t.pinned;
                            let archived = t.archived_at.is_some();
                            let purge = t.purge_at.clone();
                            let temp = t.temporary;
                            let menu = Signal::derive(move || {
                                vec![
                                    MenuItem::new(
                                        if pinned { "Unpin" } else { "Pin" },
                                        move || on_pin.run((id, !pinned)),
                                    ),
                                    MenuItem::new("Move to…", move || folders.open_move(id)),
                                    MenuItem::new(
                                        if archived { "Restore" } else { "Archive" },
                                        move || on_archive.run((id, !archived)),
                                    ),
                                ]
                                    .into_iter()
                                    .chain(super::chat_export::thread_items(id))
                                    .chain([MenuItem::new("Delete", move || on_delete.run(id)).danger()])
                                    .collect()
                            });
                            view! {
                                <div
                                    class=move || {
                                        let mut c = String::from("thread-row");
                                        if is_open() {
                                            c.push_str(" open");
                                        }
                                        if nested {
                                            c.push_str(" nested");
                                        }
                                        c
                                    }
                                    draggable=(!temp).then_some("true")
                                    on:dragstart=move |ev| {
                                        if let Some(dt) = ev.data_transfer() {
                                            let _ = dt.set_data("text/plain", &id.to_string());
                                            dt.set_effect_allowed("move");
                                        }
                                        folders.dragged.set(Some(id));
                                    }
                                    on:dragend=move |_| folders.dragged.set(None)
                                    role="button"
                                    tabindex="0"
                                    aria-current=move || is_open().then_some("true")
                                    data-dock-pick
                                    title=t.title.clone()
                                    on:click=move |_| on_open.run(id)
                                    on:keydown=move |ev| {
                                        // Not an Enter meant for the row menu inside.
                                        if ev.key() == "Enter" && ev.target() == ev.current_target() {
                                            on_open.run(id);
                                        }
                                    }
                                >
                                    <span class="thread-title">
                                        {pinned
                                            .then(|| {
                                                view! {
                                                    // Monochrome, `currentColor`, like every
                                                    // other icon in the app — the pin emoji
                                                    // rendered in its own colour, out of step
                                                    // with the sidebar's line icons (review nit).
                                                    <span class="pin-mark" title="Pinned">
                                                        <svg viewBox="0 0 16 16">
                                                            <path d="M8 1.6a3.4 3.4 0 0 0-3.4 3.4c0 2.6 3.4 8 3.4 8s3.4-5.4 3.4-8A3.4 3.4 0 0 0 8 1.6z"/>
                                                            <circle cx="8" cy="5" r="1.3"/>
                                                        </svg>
                                                    </span>
                                                }
                                            })}
                                        {t.title.clone()}
                                    </span>
                                    <span class="thread-row-menu" on:click=|ev| ev.stop_propagation()>
                                        {if temp {
                                            view! {
                                                <button
                                                    type="button"
                                                    class="thread-x"
                                                    title="Discard this temporary chat"
                                                    aria-label="Discard this temporary chat"
                                                    on:click=move |_| on_delete.run(id)
                                                >
                                                    "✕"
                                                </button>
                                            }
                                                .into_any()
                                        } else {
                                            view! { <RowMenu items=menu title="Conversation actions"/> }
                                                .into_any()
                                        }}
                                    </span>
                                    <span class="thread-meta">
                                        <span title=when_title>{when}</span>
                                        {(t.kind == "admin")
                                            .then(|| {
                                                view! {
                                                    <span
                                                        class="thread-kind"
                                                        title="Admin Chat — the lmgw self-admin tools are attached"
                                                    >
                                                        "admin"
                                                    </span>
                                                }
                                            })}
                                        <span class="thread-model" title=model_title>
                                            {model}
                                        </span>
                                        {t
                                            .agent_id
                                            .clone()
                                            .map(|a| {
                                                view! {
                                                    <span title="Opened from this agent">
                                                        {format!("· {a}")}
                                                    </span>
                                                }
                                            })}
                                        {folder
                                            .map(|f| {
                                                view! {
                                                    <span class="thread-folder-name" title="Folder">
                                                        {f}
                                                    </span>
                                                }
                                            })}
                                        {archived
                                            .then(|| {
                                                let txt = purge
                                                    .as_deref()
                                                    .map(purge_text)
                                                    .unwrap_or_else(|| "kept".to_string());
                                                view! { <span class="thread-purge">{txt}</span> }
                                            })}
                                    </span>
                                </div>
                            }
                                .into_any()
                        }
                    }}
                </For>
                <NoFolderZone env=folders/>
                <MessageHits query=query folders=folders.folders on_pick=on_pick/>
            </div>
        </div>
    }
}

#[component]
fn ThreadSettings(
    thread: RwSignal<Option<ChatThread>>,
    /// The page's: this panel comes and goes, the draft stays.
    draft: SettingsDraft,
    unsaved: Memo<bool>,
    on_save: impl Fn(Value) + Copy + Send + Sync + 'static,
    is_admin: bool,
) -> impl IntoView {
    let toasts = use_toasts();
    // Why the overrides as typed cannot be saved; shown under them and
    // blocking Save, as the server would refuse them too.
    let errors = DraftErrors::of(draft);
    let save = move |_| {
        // The page applies it to the thread, and says so, once the server
        // has taken it.
        match draft_patch(&draft) {
            Ok(body) => on_save(body),
            Err(e) => toasts.err(e),
        }
    };
    view! {
        <div class="chat-settings density-dense">
            <SettingsFields
                draft=draft
                errors=errors
                voice_resolved=Signal::derive(move || {
                    thread.with(|t| t.as_ref().and_then(|t| t.voice_resolved.clone()))
                })
                model=Signal::derive(move || {
                    thread.with(|t| t.as_ref().map(|t| t.model_alias.clone()).unwrap_or_default())
                })
                prompt_label=if is_admin {
                    "System prompt — appended to the built-in admin prompt"
                } else {
                    "System prompt"
                }
            />
            <div class="chat-settings-save">
                <Show when=move || unsaved.get()>
                    <span class="count attn">"unsaved"</span>
                    <button
                        class="link-btn"
                        title="Put this thread's saved settings back"
                        on:click=move |_| {
                            if let Some(t) = thread.get_untracked() {
                                draft.seed(&t);
                            }
                        }
                    >
                        "Discard"
                    </button>
                </Show>
                <button
                    class="btn primary"
                    disabled=move || errors.any()
                    on:click=save
                >
                    "Save"
                </button>
            </div>
        </div>
    }
}

#[component]
fn MsgView(
    m: Msg,
    /// Whether the thread's current model is known not to see images — a
    /// history image then carries a note that it will not be resent.
    #[prop(into)]
    vision_no: Signal<bool>,
    on_open: Callback<Attachment>,
    ops: MsgOps,
) -> impl IntoView {
    // The message opened for editing in place (chat_actions.rs).
    let editing = RwSignal::new(false);
    // Search hits find the message by this (`chat_search::reveal`).
    let db_id = m.db_id;
    let m_actions = m.clone();
    let m_editor = m.clone();
    let content = m.content;
    let reasoning = m.reasoning;
    let tools = m.tools;
    let streaming = m.streaming;
    let tokens = m.tokens;
    let attachments = m.attachments;
    let kb_refs = m.kb_refs;
    let context = m.context;
    let (model, answered_by, unsaved) = (m.model, m.answered_by, m.unsaved);
    let images_note = m.images_note;
    let voice = m.voice;
    let role = m.role.clone();
    let kbui = KbUi::use_ui();
    // The rendered markdown is written from an effect rather than through
    // `inner_html`, so highlighting and the toolbar always run on the DOM this
    // pass produced — no ordering race between two effects over one element.
    // Re-runs per streamed token (highlight only) and once more when the
    // message settles, which is when the toolbar and auto-detection happen.
    let md_ref: NodeRef<leptos::html::Div> = NodeRef::new();
    Effect::new(move |_| {
        let Some(el) = md_ref.get() else { return };
        let html = md_to_html(&content.get());
        // `[n]` become badges for the excerpts this answer was given.
        let html = context.with(|c| match c {
            Some(c) if !c.excerpts.is_empty() => cite_html(&html, &c.titles()),
            _ => html,
        });
        let html = voice.with(|v| chat_voice::spoken_html(html, v.as_ref(), &role));
        el.set_inner_html(&html);
        decorate_code(&el, !streaming.get());
    });
    if m.role == "user" {
        let has_images = move || attachments.with(|v| v.iter().any(|a| a.kind == "image"));
        view! {
            <div class="msg-item msg-item-user" data-mid=move || db_id.get().map(|i| i.to_string())>
            <div class="msg-user">
                <Show when=move || !attachments.with(Vec::is_empty)>
                    <div class="chip-row msg-attach-row">
                        <For each=move || attachments.get() key=|a| a.id let:a>
                            <SentChip a=a vision_no=vision_no on_open=on_open/>
                        </For>
                    </div>
                    <Show when=move || has_images() && vision_no.get()>
                        <div class="dim mini-note">
                            "the image(s) above are not sent to this model"
                        </div>
                    </Show>
                </Show>
                <Show when=move || !editing.get() && !kb_refs.with(Vec::is_empty)>
                    <div class="chip-row msg-attach-row kb-chips">
                        <For each=move || kb_refs.get() key=|id| *id let:id>
                            <KbChip id=id/>
                        </For>
                    </div>
                </Show>
                <Show when=move || !editing.get() && !content.with(String::is_empty)>
                    <div class="msg-user-text">{move || content.get()}</div>
                </Show>
                <chat_voice::MicBadge voice=voice/>
                <Show when=move || editing.get()>
                    <MsgEditor m=m_editor.clone() ops=ops editing=editing/>
                </Show>
            </div>
            <MsgActions m=m_actions.clone() ops=ops editing=editing/>
            </div>
        }
        .into_any()
    } else {
        view! {
            <div
                class="msg-item msg-assistant"
                class:msg-unsaved=move || unsaved.get()
                data-mid=move || db_id.get().map(|i| i.to_string())
            >
                <RetrievalView ctx=context/>
                <Show when=move || !reasoning.get().is_empty()>
                    <details class="reasoning">
                        <summary class="dim">"thinking"</summary>
                        <div class="reasoning-body">{move || reasoning.get()}</div>
                    </details>
                </Show>
                <For each=move || tools.get() key=|c| c.index let:card>
                    <ToolCardView card=card/>
                </For>
                <div
                    class="md"
                    node_ref=md_ref
                    style:display=move || editing.get().then_some("none")
                    on:click=move |ev| open_citation(&ev, context, kbui)
                    on:keydown=move |ev| {
                        if ev.key() == "Enter" || ev.key() == " " {
                            open_citation(&ev, context, kbui);
                        }
                    }
                ></div>
                <Show when=move || editing.get()>
                    <MsgEditor m=m_editor.clone() ops=ops editing=editing/>
                </Show>
                <Show when=move || streaming.get()>
                    <span class="caret"></span>
                </Show>
                <chat_voice::SpokenReply voice=voice/>
                {move || {
                    tokens
                        .get()
                        .map(|(p, c)| {
                            view! {
                                <div class="dim mini-note">{format!("{p} → {c} tokens")}</div>
                            }
                        })
                }}
                {move || {
                    answer_label(model.get().as_deref(), answered_by.get().as_deref())
                        .map(|l| view! { <div class="dim mini-note msg-answered-by">{l}</div> })
                }}
                {move || {
                    images_note
                        .get()
                        .map(|n| view! { <div class="mini-note msg-images-note">{n}</div> })
                }}
                <Show when=move || unsaved.get()>
                    <div class="mini-note msg-unsaved-note">{UNSAVED_NOTE}</div>
                </Show>
                <MsgActions m=m_actions.clone() ops=ops editing=editing/>
            </div>
        }
        .into_any()
    }
}

/// A click (or Enter) on a citation badge: open the excerpt it names.
fn open_citation(ev: &web_sys::Event, context: RwSignal<Option<KbContext>>, ui: KbUi) {
    let Some(n) = cite_target(ev) else { return };
    if let Some(target) = context.with_untracked(|c| c.as_ref().and_then(|c| c.source(n))) {
        ev.prevent_default();
        ui.source.set(Some(target));
    }
}

#[component]
fn ToolCardView(card: ToolCard) -> impl IntoView {
    view! {
        <details class="tool-card" class:err=move || card.is_error.get()>
            <summary>
                <span class="mono-sm">{move || card.name.get()}</span>
                {move || {
                    if card.done.get() {
                        // Replayed IR cards carry no duration — say "done"
                        // rather than nothing at all.
                        card.ms
                            .get()
                            .map(|ms| format!(" · {ms} ms"))
                            .unwrap_or_else(|| " · done".to_string())
                    } else {
                        " · running…".to_string()
                    }
                }}
            </summary>
            <div class="tool-io">
                <div class="dim mini-note">"arguments"</div>
                <pre class="preset">{move || card.args.get()}</pre>
                <Show when=move || !card.output.get().is_empty()>
                    <div class="dim mini-note">"output"</div>
                    <pre class="preset">{move || card.output.get()}</pre>
                </Show>
            </div>
        </details>
    }
}

#[component]
fn StatsRow(#[prop(into)] stats: Signal<Option<Stats>>) -> impl IntoView {
    view! {
        {move || {
            stats
                .get()
                .map(|s| {
                    let tilde = if s.server { "" } else { "~" };
                    let mut parts: Vec<String> = Vec::new();
                    if let Some(v) = s.ttft_ms {
                        parts.push(format!("ttft {:.0} ms", v));
                    }
                    if let Some(v) = s.total_ms {
                        parts.push(format!("total {:.1} s", v / 1000.0));
                    }
                    if let Some(v) = s.tps {
                        parts.push(format!("{tilde}{v:.1} tok/s"));
                    }
                    if let Some(v) = s.prefill {
                        parts.push(format!("prefill {tilde}{v:.0} tok/s"));
                    }
                    if let Some(v) = s.cached {
                        parts.push(format!("{v} cached"));
                    }
                    if let (Some(n), Some(a)) = (s.draft_n, s.draft_accepted) {
                        if n > 0 {
                            parts.push(format!(
                                "spec {:.0}%",
                                a as f64 / n as f64 * 100.0
                            ));
                        }
                    }
                    let ctx = match (s.prompt_tokens, s.completion_tokens, s.ctx_max) {
                        (Some(p), Some(c), Some(max)) if max > 0 => {
                            Some(((p + c) as f64 / max as f64, p + c, max))
                        }
                        _ => None,
                    };
                    let ignored = (!s.ignored.is_empty())
                        .then(|| {
                            let names = s
                                .ignored
                                .iter()
                                .map(|k| if k == "enabled" { "on/off" } else { k.as_str() })
                                .collect::<Vec<_>>()
                                .join(", ");
                            let title = match &s.reasoning_note {
                                Some(note) => format!("{note}. Reasoning off was not applied as asked ({names})."),
                                None => format!(
                                    "The route that answered has no field for this thread's reasoning or sampling override ({names}) — the model never saw it",
                                ),
                            };
                            view! {
                                <span
                                    class="type-badge fallback-badge"
                                    title=title
                                >
                                    {format!("not sent: {names}")}
                                </span>
                            }
                        });
                    view! {
                        <div class="stats-row" class:live=s.live>
                            <span class="mono-sm dim">{parts.join(" · ")}</span>
                            {ignored}
                            {ctx
                                .map(|(frac, used, max)| {
                                    view! {
                                        <span
                                            class="ctx-bar"
                                            title=format!("context: {used} / {max}")
                                        >
                                            <i style=format!(
                                                "width:{}%",
                                                (frac * 100.0).min(100.0),
                                            )></i>
                                        </span>
                                    }
                                })}
                        </div>
                    }
                })
        }}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the server took is what the page holds after: only the patch's
    /// keys, and a null clears a number back to "model default".
    #[test]
    fn a_saved_patch_is_applied_to_the_thread_key_by_key() {
        let mut t = ChatThread {
            model_alias: "a".into(),
            system_prompt: "old".into(),
            temperature: Some(0.2),
            max_tokens: Some(100),
            ..Default::default()
        };
        apply_settings(&mut t, &json!({ "model_alias": "b" }));
        assert_eq!(
            (
                t.model_alias.as_str(),
                t.system_prompt.as_str(),
                t.temperature
            ),
            ("b", "old", Some(0.2))
        );
        apply_settings(
            &mut t,
            &json!({ "system_prompt": "new", "temperature": null, "max_tokens": 5, "mcp_tools": [] }),
        );
        assert_eq!(
            (t.system_prompt.as_str(), t.temperature, t.max_tokens),
            ("new", None, Some(5))
        );
        apply_settings(
            &mut t,
            &json!({ "reasoning_enabled": false, "reasoning_effort": null, "reasoning_budget": 0 }),
        );
        assert_eq!(
            (
                t.reasoning_enabled,
                t.reasoning_effort.as_deref(),
                t.reasoning_budget
            ),
            (Some(false), None, Some(0))
        );
    }

    #[test]
    fn thread_settings_are_unsaved_only_when_a_save_would_change_them() {
        let t = ChatThread {
            system_prompt: "Be kind".into(),
            temperature: Some(0.7),
            reasoning_enabled: Some(true),
            reasoning_budget: Some(512),
            ..Default::default()
        };
        let stored = SettingsText::of(&t);
        assert_eq!(
            (
                stored.temp.as_str(),
                stored.think.as_str(),
                stored.budget.as_str()
            ),
            ("0.7", "on", "512")
        );
        assert!(!stored.differs(&stored));
        let padded = SettingsText {
            temp: " 0.7 ".into(),
            max_tok: " ".into(),
            budget: "512 ".into(),
            ..stored.clone()
        };
        assert!(!padded.differs(&stored));
        let typed = SettingsText {
            sys: "Be terse".into(),
            ..stored.clone()
        };
        assert!(typed.differs(&stored));
        let other_effort = SettingsText {
            effort: "high".into(),
            ..stored.clone()
        };
        assert!(other_effort.differs(&stored));
        // Saved as typed, stored as the number it is.
        let spelled = SettingsText {
            temp: "0.70".into(),
            budget: "0512".into(),
            ..stored.clone()
        };
        assert!(!spelled.differs(&stored));
    }

    fn thread(id: i64) -> ChatThread {
        ChatThread {
            id,
            ..Default::default()
        }
    }

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

    #[test]
    fn day_numbers_count_days_across_months_and_leap_years() {
        assert_eq!(day_number("1970-01-01"), Some(0));
        assert_eq!(day_number("2026-09-24"), Some(20_720));
        assert_eq!(
            day_number("2024-03-01").unwrap() - day_number("2024-02-28").unwrap(),
            2
        );
        assert_eq!(day_number("2026-13-01"), None);
        assert_eq!(day_number("garbage"), None);
    }

    #[test]
    fn threads_file_under_today_yesterday_the_week_then_their_month() {
        let today = "2026-09-24";
        assert_eq!(day_group("2026-09-24", today), "Today");
        assert_eq!(day_group("2026-09-25", today), "Today");
        assert_eq!(day_group("2026-09-23", today), "Yesterday");
        assert_eq!(day_group("2026-09-18", today), "Previous 7 days");
        assert_eq!(day_group("2026-09-17", today), "September 2026");
        assert_eq!(day_group("2026-08-31", today), "August 2026");
        assert_eq!(day_group("2025-12-31", "2026-01-02"), "Previous 7 days");
        assert_eq!(day_group("", today), "Undated");
    }

    #[test]
    fn deep_link_opens_the_named_thread_when_it_exists() {
        let list = vec![thread(7), thread(3)];
        assert_eq!(seed_thread(Some(3), &list), Some(3));
    }

    #[test]
    fn deep_link_opens_the_named_thread_even_off_the_active_list() {
        // Not in the (active-only) list a cold load fetches — an archived
        // thread, or one deleted since — still wins over the first active
        // thread (finding #7).
        let list = vec![thread(7), thread(3)];
        assert_eq!(seed_thread(Some(99), &list), Some(99));
    }

    #[test]
    fn no_deep_link_falls_back_to_the_first_thread() {
        let list = vec![thread(7), thread(3)];
        assert_eq!(seed_thread(None, &list), Some(7));
        assert_eq!(seed_thread(Some(3), &[]), Some(3));
        assert_eq!(seed_thread(None, &[]), None);
    }
}
