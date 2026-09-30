//! Knowledge bases in the Chat (chat-complete §9.3), the parts around the
//! composer and the settings: the thread's (or folder's) Knowledge section,
//! the `#` picker and its chips, and the list of bases they share. What a turn
//! retrieved, and the citations, are [`super::chat_retrieval`].

use leptos::html;
use leptos::prelude::*;
use lmgw_api_types::SettingsFull;
use serde::Deserialize;
use serde_json::{json, Value};

use super::chat::ChatThread;
use super::knowledge::Kb;
use super::knowledge_source::SourceRef;
use crate::scope::Scope;
use crate::widgets::Popover;

// ---------------------------------------------------------------------------
// The shared list of bases
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct BasesResponse {
    bases: Vec<Kb>,
}

/// The Chat page's knowledge state: the bases (for pickers and chip names)
/// and the one source viewer citations and excerpts open. Provided as context
/// by the page.
#[derive(Clone, Copy)]
pub(super) struct KbUi {
    pub bases: RwSignal<Vec<Kb>>,
    /// `None` until the first read answers.
    pub loaded: RwSignal<Option<Result<(), String>>>,
    pub source: RwSignal<Option<SourceRef>>,
    scope: Scope,
}

impl KbUi {
    /// Made by the page: reads the bases now and provides itself.
    pub(super) fn provide(scope: Scope) -> Self {
        let ui = Self {
            bases: RwSignal::new(Vec::new()),
            loaded: RwSignal::new(None),
            source: RwSignal::new(None),
            scope,
        };
        provide_context(ui);
        ui.reload();
        ui
    }

    pub(super) fn use_ui() -> Self {
        expect_context::<Self>()
    }

    /// Read the bases again; a failed read keeps the list and says so.
    pub(super) fn reload(&self) {
        let ui = *self;
        self.scope.spawn(async move {
            match crate::api::get::<BasesResponse>("/api/knowledge/bases").await {
                Ok(r) => {
                    ui.bases.try_set(r.bases);
                    ui.loaded.try_set(Some(Ok(())));
                }
                Err(e) => {
                    ui.loaded.try_set(Some(Err(e.to_string())));
                }
            }
        });
    }

    fn name(&self, id: i64) -> Option<String> {
        self.bases
            .with(|b| b.iter().find(|k| k.id == id).map(|k| k.name.clone()))
    }

    /// Whether the list is known and lacks `id`: the base was deleted.
    fn is_gone(&self, id: i64) -> bool {
        self.loaded.with(|l| matches!(l, Some(Ok(()))))
            && self.bases.with(|b| b.iter().all(|k| k.id != id))
    }
}

/// Why a base may not answer well right now, for a warning beside it.
fn base_warning(k: &Kb) -> Option<String> {
    if !k.embed_resolvable {
        Some("its embedding model is unavailable — keyword search only".into())
    } else if k.counts.ready == 0 {
        Some("no file is ready yet".into())
    } else if k.counts.ready < k.counts.files {
        Some(format!(
            "{} of {} files are ready",
            k.counts.ready, k.counts.files
        ))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Thread and folder settings
// ---------------------------------------------------------------------------

/// The Knowledge section's boxes, held by the page like the other drafts.
#[derive(Clone, Copy)]
pub(super) struct KbDraft {
    pub ids: RwSignal<Vec<i64>>,
    /// `"auto"` or `"tool"`.
    pub mode: RwSignal<String>,
    /// Blank = the `chat_kb_budget_tokens` setting.
    pub budget: RwSignal<String>,
}

fn mode_of(t: &ChatThread) -> &'static str {
    if t.kb_mode == "tool" {
        "tool"
    } else {
        "auto"
    }
}

impl KbDraft {
    pub(super) fn new() -> Self {
        Self {
            ids: RwSignal::new(Vec::new()),
            mode: RwSignal::new("auto".into()),
            budget: RwSignal::new(String::new()),
        }
    }

    pub(super) fn seed(&self, t: &ChatThread) {
        self.ids.set(t.kb_ids.clone());
        self.mode.set(mode_of(t).into());
        self.budget.set(
            t.kb_budget_tokens
                .map(|v| v.to_string())
                .unwrap_or_default(),
        );
    }

    /// Does the form differ from the thread as stored? (tracked)
    pub(super) fn differs_from(&self, t: &ChatThread) -> bool {
        let budget = self.budget.get();
        let budget = budget.trim();
        let same_budget = match (budget.parse::<i64>(), t.kb_budget_tokens) {
            (Ok(a), Some(b)) => a == b,
            (Err(_), None) => budget.is_empty(),
            _ => false,
        };
        self.ids.with(|i| *i != t.kb_ids) || self.mode.get() != mode_of(t) || !same_budget
    }

    /// The three keys of a settings body, or why the boxes cannot be saved.
    pub(super) fn patch(&self) -> Result<Value, String> {
        let budget = match self.budget.get_untracked().trim() {
            "" => None,
            s => match s.parse::<i64>() {
                Ok(n) if n > 0 => Some(n),
                _ => return Err("the excerpt budget must be a whole number above 0".into()),
            },
        };
        Ok(json!({
            "kb_ids": self.ids.get_untracked(),
            "kb_mode": self.mode.get_untracked(),
            "kb_budget_tokens": budget,
        }))
    }
}

/// A settings patch the server took, applied to the thread as the page holds
/// it: only the keys it carries.
pub(super) fn apply_kb(t: &mut ChatThread, body: &Value) {
    if let Some(v) = body.get("kb_ids") {
        t.kb_ids = serde_json::from_value(v.clone()).unwrap_or_default();
    }
    if let Some(v) = body.get("kb_mode").and_then(Value::as_str) {
        t.kb_mode = v.to_string();
    }
    if let Some(v) = body.get("kb_budget_tokens") {
        t.kb_budget_tokens = v.as_i64();
    }
}

/// The chat head's badge: what knowledge does for this thread.
pub(super) fn kb_badge(t: &ChatThread) -> Option<String> {
    if t.kb_ids.is_empty() {
        return None;
    }
    Some(if t.kb_mode == "tool" {
        "kb: tool".to_string()
    } else {
        format!("{} kb", t.kb_ids.len())
    })
}

/// The Knowledge section of the settings form: which bases, how they are
/// used, and the excerpt budget.
#[component]
pub(super) fn KbSection(kb: KbDraft) -> impl IntoView {
    let ui = KbUi::use_ui();
    ui.reload();
    let default_budget =
        LocalResource::new(|| crate::api::get::<SettingsFull>("/api/settings-full"));
    let KbDraft { ids, mode, budget } = kb;
    let toggle = move |id: i64, on: bool| {
        ids.update(|v| {
            v.retain(|x| *x != id);
            if on {
                v.push(id);
            }
        });
    };
    let gone = Memo::new(move |_| {
        ids.get()
            .into_iter()
            .filter(|id| ui.is_gone(*id))
            .collect::<Vec<_>>()
    });
    let is_tool = move || mode.get() == "tool";
    view! {
        <div class="field kb-section">
            <label>"Knowledge"</label>
            {move || match ui.loaded.get() {
                None => view! { <div class="dim">"Loading…"</div> }.into_any(),
                Some(Err(e)) => {
                    view! { <div class="dim">"Knowledge bases unavailable: " {e}</div> }.into_any()
                }
                Some(Ok(())) if ui.bases.with(Vec::is_empty) && ids.with(Vec::is_empty) => {
                    view! {
                        <div class="dim">
                            "No knowledge bases yet — create one on the Knowledge page."
                        </div>
                    }
                        .into_any()
                }
                Some(Ok(())) => ().into_any(),
            }}
            <For each=move || ui.bases.get() key=|k| k.id let:k>
                {
                    let id = k.id;
                    let warn = base_warning(&k);
                    view! {
                        <label class="row dim kb-pick">
                            <input
                                type="checkbox"
                                prop:checked=move || ids.with(|v| v.contains(&id))
                                on:change=move |ev| toggle(id, event_target_checked(&ev))
                            />
                            <span class="mono">{k.name.clone()}</span>
                            <span class="type-badge">{crate::fmt::count_of(k.counts.files as usize, "files")}</span>
                            {warn
                                .map(|w| {
                                    view! {
                                        <span class="kb-warn" title=w.clone()>
                                            "⚠ " {w.clone()}
                                        </span>
                                    }
                                })}
                        </label>
                    }
                }
            </For>
            <For each=move || gone.get() key=|id| *id let:id>
                <div class="row dim kb-pick kb-gone">
                    <span class="mono">{format!("base #{id}")}</span>
                    <span class="type-badge">"deleted"</span>
                    <button
                        type="button"
                        class="chip-x"
                        title="Remove it from this list"
                        on:click=move |_| toggle(id, false)
                    >
                        "✕"
                    </button>
                </div>
            </For>
            <div class="kb-mode">
                <div class="seg">
                    <button
                        type="button"
                        class="seg-btn"
                        class:active=move || !is_tool()
                        on:click=move |_| mode.set("auto".into())
                    >
                        "Auto"
                    </button>
                    <button
                        type="button"
                        class="seg-btn"
                        class:active=is_tool
                        on:click=move |_| mode.set("tool".into())
                    >
                        "Tool"
                    </button>
                </div>
                <div class="dim mini-note">
                    {move || {
                        if is_tool() {
                            "Tool: the model searches when it decides to. Needs a model that calls tools; the search is limited to the bases ticked here."
                        } else {
                            "Auto: the best excerpts are searched and added to each turn. Works with any model."
                        }
                    }}
                </div>
            </div>
            <Show when=move || !is_tool()>
                <div class="field-grid" style="--field-min:140px">
                    <div class="field">
                        <label>"Excerpt budget (tokens)"</label>
                        <input
                            class="input mono"
                            inputmode="numeric"
                            placeholder=move || {
                                match default_budget.get() {
                                    Some(Ok(s)) => format!("{} (setting)", s.chat_kb_budget_tokens),
                                    _ => "the setting".to_string(),
                                }
                            }
                            prop:value=move || budget.get()
                            on:input=move |ev| budget.set(event_target_value(&ev))
                        />
                    </div>
                </div>
            </Show>
        </div>
    }
}

// ---------------------------------------------------------------------------
// Chips
// ---------------------------------------------------------------------------

/// One knowledge-base chip: on a sent message (read-only) or in the composer
/// and the message editor (with ✕).
#[component]
pub(super) fn KbChip(id: i64, #[prop(optional)] on_remove: Option<Callback<i64>>) -> impl IntoView {
    let ui = KbUi::use_ui();
    let label = move || ui.name(id).unwrap_or_else(|| format!("base #{id}"));
    view! {
        <span
            class="chip attach-chip kb-chip"
            class:warn=move || ui.is_gone(id)
            title=move || {
                if ui.is_gone(id) { "This knowledge base was deleted".to_string() } else { format!("Knowledge base: {}", label()) }
            }
        >
            <span class="kb-chip-tag">"kb"</span>
            <span class="chip-name">{label}</span>
            <Show when=move || ui.is_gone(id)>
                <span class="chip-warn-mark">"deleted"</span>
            </Show>
            {on_remove
                .map(|cb| {
                    view! {
                        <button
                            type="button"
                            class="chip-x"
                            title="Remove"
                            on:click=move |_| cb.run(id)
                        >
                            "✕"
                        </button>
                    }
                })}
        </span>
    }
}

/// The composer's row of picked bases, this message only.
#[component]
pub(super) fn KbDraftChips(chips: RwSignal<Vec<i64>>) -> impl IntoView {
    view! {
        <Show when=move || !chips.with(Vec::is_empty)>
            <div class="chip-row draft-chips kb-chips">
                <For each=move || chips.get() key=|id| *id let:id>
                    <KbChip
                        id=id
                        on_remove=Callback::new(move |id| chips.update(|v| v.retain(|x| *x != id)))
                    />
                </For>
            </div>
        </Show>
    }
}

// ---------------------------------------------------------------------------
// The `#` picker
// ---------------------------------------------------------------------------

/// A `#query` being typed at a word start: byte range `start..end` of the
/// text (the `#` through the caret) and what follows the `#`.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct HashToken {
    pub start: usize,
    pub end: usize,
    pub query: String,
}

/// The `#` token the caret (a byte offset) is at the end of, if any: the
/// word before the caret starts with a single `#` and follows whitespace or
/// the start of the text. `C#` and `##` are not one.
pub(super) fn hash_token(text: &str, cursor: usize) -> Option<HashToken> {
    if cursor > text.len() || !text.is_char_boundary(cursor) {
        return None;
    }
    let before = &text[..cursor];
    let start = before
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    let query = before[start..].strip_prefix('#')?;
    if query.contains('#') {
        return None;
    }
    Some(HashToken {
        start,
        end: cursor,
        query: query.to_string(),
    })
}

/// The text with the token cut out (one of two joining spaces goes with it),
/// and where the caret belongs afterwards, as a byte offset.
pub(super) fn cut_token(text: &str, t: &HashToken) -> (String, usize) {
    let mut tail = &text[t.end..];
    if text[..t.start].ends_with(' ') && tail.starts_with(' ') {
        tail = &tail[1..];
    }
    (format!("{}{tail}", &text[..t.start]), t.start)
}

fn utf16_to_byte(text: &str, units: usize) -> usize {
    let mut seen = 0;
    for (i, c) in text.char_indices() {
        if seen >= units {
            return i;
        }
        seen += c.len_utf16();
    }
    text.len()
}

fn byte_to_utf16(text: &str, byte: usize) -> usize {
    text[..byte.min(text.len())].encode_utf16().count()
}

/// One row of the picker.
#[derive(Clone, PartialEq)]
struct PickRow {
    id: i64,
    name: String,
    files: i64,
    warn: Option<String>,
}

/// The composer's picker state. Made by the page; the textarea, the button
/// and the popover call into it.
#[derive(Clone, Copy)]
pub(super) struct KbPick {
    open: RwSignal<bool>,
    token: RwSignal<Option<HashToken>>,
    sel: RwSignal<usize>,
    /// Opened by the button, not by a `#`: focus is in the list, not the text.
    button: RwSignal<bool>,
    /// A `#` the owner closed with Esc stays closed until it is a new one.
    dismissed: RwSignal<Option<usize>>,
    rows: Memo<Vec<PickRow>>,
    chips: RwSignal<Vec<i64>>,
    composer: RwSignal<String>,
    ta: NodeRef<html::Textarea>,
    ui: KbUi,
}

impl KbPick {
    pub(super) fn new(
        chips: RwSignal<Vec<i64>>,
        composer: RwSignal<String>,
        ta: NodeRef<html::Textarea>,
    ) -> Self {
        let ui = KbUi::use_ui();
        let token = RwSignal::new(None::<HashToken>);
        let button = RwSignal::new(false);
        let rows = Memo::new(move |_| {
            let q = if button.get() {
                String::new()
            } else {
                token
                    .with(|t| t.as_ref().map(|t| t.query.to_lowercase()))
                    .unwrap_or_default()
            };
            let picked = chips.get();
            ui.bases.with(|b| {
                b.iter()
                    .filter(|k| !picked.contains(&k.id) && k.name.to_lowercase().contains(&q))
                    .map(|k| PickRow {
                        id: k.id,
                        name: k.name.clone(),
                        files: k.counts.files,
                        warn: base_warning(k),
                    })
                    .collect()
            })
        });
        Self {
            open: RwSignal::new(false),
            token,
            sel: RwSignal::new(0),
            button,
            dismissed: RwSignal::new(None),
            rows,
            chips,
            composer,
            ta,
            ui,
        }
    }

    /// Follow the caret: open for a `#` token with something to offer.
    fn follow(&self, ta: &web_sys::HtmlTextAreaElement) {
        let text = ta.value();
        let cursor = ta
            .selection_start()
            .ok()
            .flatten()
            .map(|u| utf16_to_byte(&text, u as usize))
            .unwrap_or(text.len());
        let tok = hash_token(&text, cursor);
        if self.token.get_untracked() == tok {
            return;
        }
        if tok.is_some() && self.token.get_untracked().is_none() {
            self.ui.reload();
        }
        self.sel.set(0);
        self.button.set(false);
        self.token.set(tok.clone());
        let want = tok.as_ref().is_some_and(|t| {
            self.dismissed.get_untracked() != Some(t.start) && !self.rows.get_untracked().is_empty()
        });
        if tok.is_none() {
            self.dismissed.set(None);
        }
        if self.open.get_untracked() != want {
            self.open.set(want);
        }
    }

    /// The textarea's `input`.
    pub(super) fn on_input(&self, ev: &web_sys::Event) {
        let ta: web_sys::HtmlTextAreaElement = event_target(ev);
        self.composer.set(ta.value());
        self.follow(&ta);
    }

    /// The caret moved without typing (arrows, a click).
    pub(super) fn on_caret(&self, ev: &web_sys::Event) {
        let ta: web_sys::HtmlTextAreaElement = event_target(ev);
        self.follow(&ta);
    }

    /// Up/Down/Enter/Tab/Esc while the picker is open; `true` when it took
    /// the key.
    fn nav_key(&self, ev: &web_sys::KeyboardEvent) -> bool {
        if !self.open.get_untracked() || ev.is_composing() {
            return false;
        }
        let n = self.rows.with_untracked(Vec::len);
        let at = self.sel.get_untracked();
        match ev.key().as_str() {
            "ArrowDown" if n > 0 => self.sel.set((at + 1) % n),
            "ArrowUp" if n > 0 => self.sel.set((at + n - 1) % n),
            "Enter" | "Tab" if !ev.shift_key() && n > 0 => {
                if let Some(id) = self
                    .rows
                    .with_untracked(|r| r.get(at.min(n - 1)).map(|r| r.id))
                {
                    self.pick(id);
                }
            }
            "Escape" => {
                self.dismissed
                    .set(self.token.get_untracked().map(|t| t.start));
                self.open.set(false);
                self.button.set(false);
            }
            _ => return false,
        }
        ev.prevent_default();
        ev.stop_propagation();
        true
    }

    /// The textarea's `keydown`: `true` when the picker took the key.
    pub(super) fn on_key(&self, ev: &web_sys::KeyboardEvent) -> bool {
        !self.button.get_untracked() && self.nav_key(ev)
    }

    /// The button beside Attach.
    pub(super) fn toggle_button(&self) {
        if self.open.get_untracked() {
            self.open.set(false);
            return;
        }
        self.ui.reload();
        self.sel.set(0);
        self.button.set(true);
        self.open.set(true);
    }

    /// Add base `id` to this message; a `#query` is cut out of the text.
    fn pick(&self, id: i64) {
        self.chips.update(|v| {
            if !v.contains(&id) {
                v.push(id);
            }
        });
        if !self.button.get_untracked() {
            if let Some(t) = self.token.get_untracked() {
                let text = self.composer.get_untracked();
                if t.end <= text.len() {
                    let (cut, at) = cut_token(&text, &t);
                    let units = byte_to_utf16(&cut, at) as u32;
                    self.composer.set(cut);
                    let ta = self.ta;
                    // After the value binding has written the new text.
                    request_animation_frame(move || {
                        if let Some(el) = ta.get_untracked() {
                            let _ = el.set_selection_range(units, units);
                        }
                    });
                }
            }
        }
        self.token.set(None);
        self.dismissed.set(None);
        self.button.set(false);
        self.open.set(false);
        if let Some(el) = self.ta.get_untracked() {
            let _ = el.focus();
        }
    }
}

/// The picker's list, anchored to the composer.
#[component]
pub(super) fn KbPopover(pick: KbPick, anchor: NodeRef<html::Div>) -> impl IntoView {
    view! {
        <Popover open=pick.open anchor=anchor class="kb-pop" min_width=280>
            <div
                class="pop-list"
                role="listbox"
                tabindex="-1"
                attr:data-autofocus=move || pick.button.get().then_some("")
                on:keydown=move |ev| {
                    pick.nav_key(&ev);
                }
            >
                {move || {
                    let rows = pick.rows.get();
                    if rows.is_empty() {
                        let none = pick.ui.bases.with(Vec::is_empty);
                        return view! {
                            <div class="pop-empty">
                                {if none {
                                    "No knowledge bases yet — create one on the Knowledge page."
                                } else {
                                    "No more bases match."
                                }}
                            </div>
                        }
                            .into_any();
                    }
                    rows.into_iter()
                        .enumerate()
                        .map(|(i, r)| {
                            let id = r.id;
                            view! {
                                <div
                                    class="mp-row"
                                    class:active=move || pick.sel.get() == i
                                    role="option"
                                    aria-selected=move || (pick.sel.get() == i).to_string()
                                    on:mousedown=|ev| ev.prevent_default()
                                    on:pointermove=move |_| {
                                        if pick.sel.get_untracked() != i {
                                            pick.sel.set(i)
                                        }
                                    }
                                    on:click=move |_| pick.pick(id)
                                >
                                    <span class="mp-id">{r.name}</span>
                                    <span class="mp-meta">
                                        <span class="mp-badge">{crate::fmt::count_of(r.files as usize, "files")}</span>
                                        {r
                                            .warn
                                            .map(|w| {
                                                view! { <span class="kb-warn" title=w>"⚠"</span> }
                                            })}
                                    </span>
                                </div>
                            }
                        })
                        .collect_view()
                        .into_any()
                }}
            </div>
        </Popover>
    }
}

/// The button beside Attach.
#[component]
pub(super) fn KbButton(pick: KbPick) -> impl IntoView {
    view! {
        <button
            type="button"
            class="btn ghost composer-attach composer-kb"
            title="Add knowledge to this message (or type #)"
            on:mousedown=|ev| ev.prevent_default()
            on:click=move |_| pick.toggle_button()
        >
            <svg viewBox="0 0 16 16">
                <path d="M3 2.5h7.5a2 2 0 0 1 2 2v9H5a2 2 0 0 1-2-2z M3 11.5a2 2 0 0 1 2-2h7.5"/>
            </svg>
        </button>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(text: &str, cursor: usize) -> Option<(usize, usize, String)> {
        hash_token(text, cursor).map(|t| (t.start, t.end, t.query))
    }

    #[test]
    fn a_hash_at_a_word_start_is_a_token() {
        assert_eq!(tok("#", 1), Some((0, 1, "".into())));
        assert_eq!(tok("#tax", 4), Some((0, 4, "tax".into())));
        assert_eq!(tok("see #tax", 8), Some((4, 8, "tax".into())));
        assert_eq!(tok("a\n#t", 4), Some((2, 4, "t".into())));
    }

    #[test]
    fn the_token_ends_at_the_caret() {
        assert_eq!(tok("#taxes now", 4), Some((0, 4, "tax".into())));
    }

    #[test]
    fn a_hash_inside_a_word_or_doubled_is_not_one() {
        assert_eq!(tok("C#", 2), None);
        assert_eq!(tok("a#b", 3), None);
        assert_eq!(tok("##", 2), None);
        assert_eq!(tok("#a#b", 4), None);
    }

    #[test]
    fn a_finished_word_is_not_one() {
        assert_eq!(tok("#tax ", 5), None);
        assert_eq!(tok("# heading", 9), None);
        assert_eq!(tok("plain text", 5), None);
    }

    #[test]
    fn multibyte_text_keeps_its_boundaries() {
        let t = "äö #steuer";
        assert_eq!(tok(t, t.len()), Some((5, t.len(), "steuer".into())));
        assert_eq!(hash_token("äö", 1), None, "not on a character boundary");
    }

    #[test]
    fn cutting_the_token_leaves_one_space() {
        let text = "what is in #tax about rent";
        let t = hash_token(text, 15).unwrap();
        assert_eq!(cut_token(text, &t), ("what is in about rent".into(), 11));
        let text = "#tax rent";
        let t = hash_token(text, 4).unwrap();
        assert_eq!(cut_token(text, &t), (" rent".into(), 0));
        let text = "question #a";
        let t = hash_token(text, text.len()).unwrap();
        assert_eq!(cut_token(text, &t), ("question ".into(), 9));
    }

    #[test]
    fn utf16_offsets_convert_both_ways() {
        let t = "a😀b";
        assert_eq!(utf16_to_byte(t, 3), 5);
        assert_eq!(byte_to_utf16(t, 5), 3);
        assert_eq!(utf16_to_byte(t, 99), t.len());
    }

    #[test]
    fn the_budget_box_is_a_positive_whole_number_or_blank() {
        let owner = Owner::new();
        owner.with(|| {
            let d = KbDraft::new();
            assert_eq!(d.patch().unwrap()["kb_budget_tokens"], Value::Null);
            d.budget.set(" 2500 ".into());
            assert_eq!(d.patch().unwrap()["kb_budget_tokens"], json!(2500));
            for bad in ["0", "-3", "1.5", "lots"] {
                d.budget.set(bad.into());
                assert!(d.patch().is_err(), "{bad}");
            }
        });
    }

    #[test]
    fn a_saved_patch_lands_on_the_thread() {
        let mut t = ChatThread::default();
        apply_kb(
            &mut t,
            &json!({"kb_ids": [3, 4], "kb_mode": "tool", "kb_budget_tokens": 900}),
        );
        assert_eq!(
            (t.kb_ids.clone(), t.kb_mode.as_str(), t.kb_budget_tokens),
            (vec![3, 4], "tool", Some(900))
        );
        assert_eq!(kb_badge(&t).as_deref(), Some("kb: tool"));
        apply_kb(&mut t, &json!({"kb_ids": [], "kb_budget_tokens": null}));
        assert_eq!((t.kb_ids.len(), t.kb_budget_tokens), (0, None));
        assert_eq!(kb_badge(&t), None);
    }
}
