//! The fields of a knowledge base as preference groups, shared by the New
//! dialog and the Settings tab: one definition of what a base is made of.
//! Each row carries the accent dot while it differs from the form's
//! baseline, the base as last saved or read.

use leptos::prelude::*;
use serde_json::{json, Value};

use crate::catalog::{use_model_catalog, CatalogEntry};
use crate::widgets::{EntryRow, ModelPicker, NumberRow, PrefGroup, StackRow, SwitchRow};

use super::knowledge::Kb;

/// A base being typed. Numbers stay text until they are sent, so an empty or
/// half-typed one is a message, not a silent default. `baseline` is the base
/// as the form takes it to be saved: a row differing from it carries the
/// dot.
#[derive(Clone, Copy)]
pub struct KbForm {
    pub name: RwSignal<String>,
    pub description: RwSignal<String>,
    pub embed: RwSignal<String>,
    pub rerank: RwSignal<String>,
    pub vision: RwSignal<String>,
    pub chunk_tokens: RwSignal<String>,
    pub chunk_overlap: RwSignal<String>,
    pub mcp_visible: RwSignal<bool>,
    baseline: RwSignal<KbValues>,
}

/// The form's values as plain data: what is typed, what a save sent, the
/// base as read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KbValues {
    pub name: String,
    pub description: String,
    pub embed: String,
    pub rerank: String,
    pub vision: String,
    pub chunk_tokens: String,
    pub chunk_overlap: String,
    pub mcp_visible: bool,
}

impl KbValues {
    pub fn of(kb: &Kb) -> Self {
        Self {
            name: kb.name.clone(),
            description: kb.description.clone(),
            embed: kb.embed_alias.clone(),
            rerank: kb.rerank_alias.clone(),
            vision: kb.vision_alias.clone(),
            chunk_tokens: kb.chunk_tokens.to_string(),
            chunk_overlap: kb.chunk_overlap.to_string(),
            mcp_visible: kb.mcp_visible,
        }
    }

    /// What differs from `base`, field by field, as the form compares them:
    /// trimmed text.
    pub fn dirty_against(&self, base: &Self) -> KbDirty {
        KbDirty {
            name: differs(&self.name, &base.name),
            description: differs(&self.description, &base.description),
            embed: differs(&self.embed, &base.embed),
            rerank: differs(&self.rerank, &base.rerank),
            vision: differs(&self.vision, &base.vision),
            chunk_tokens: differs(&self.chunk_tokens, &base.chunk_tokens),
            chunk_overlap: differs(&self.chunk_overlap, &base.chunk_overlap),
            mcp_visible: self.mcp_visible != base.mcp_visible,
        }
    }

    /// The form after the baseline moved from `old` to `new`: a field still
    /// at `old` follows to `new` (someone else's change shows), one changed
    /// since stays as typed (and now differs from `new`, or not).
    pub fn follow(&self, old: &Self, new: &Self) -> Self {
        let pick = |t: &String, o: &String, n: &String| {
            if differs(t, o) {
                t.clone()
            } else {
                n.clone()
            }
        };
        Self {
            name: pick(&self.name, &old.name, &new.name),
            description: pick(&self.description, &old.description, &new.description),
            embed: pick(&self.embed, &old.embed, &new.embed),
            rerank: pick(&self.rerank, &old.rerank, &new.rerank),
            vision: pick(&self.vision, &old.vision, &new.vision),
            chunk_tokens: pick(&self.chunk_tokens, &old.chunk_tokens, &new.chunk_tokens),
            chunk_overlap: pick(&self.chunk_overlap, &old.chunk_overlap, &new.chunk_overlap),
            mcp_visible: if self.mcp_visible != old.mcp_visible {
                self.mcp_visible
            } else {
                new.mcp_visible
            },
        }
    }
}

/// Which fields differ from the base as saved, by name.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KbDirty {
    pub name: bool,
    pub description: bool,
    pub embed: bool,
    pub rerank: bool,
    pub vision: bool,
    pub chunk_tokens: bool,
    pub chunk_overlap: bool,
    pub mcp_visible: bool,
}

impl KbDirty {
    pub fn count(&self) -> usize {
        [
            self.name,
            self.description,
            self.embed,
            self.rerank,
            self.vision,
            self.chunk_tokens,
            self.chunk_overlap,
            self.mcp_visible,
        ]
        .iter()
        .filter(|d| **d)
        .count()
    }
}

/// `typed` against `saved`, as the form compares them: trimmed text.
fn differs(typed: &str, saved: &str) -> bool {
    typed.trim() != saved.trim()
}

impl KbForm {
    /// A new base: the backend's defaults (512 / 64, visible on /mcp).
    pub fn blank() -> Self {
        let v = KbValues {
            chunk_tokens: "512".into(),
            chunk_overlap: "64".into(),
            mcp_visible: true,
            ..Default::default()
        };
        Self::with(v)
    }

    pub fn of(kb: &Kb) -> Self {
        Self::with(KbValues::of(kb))
    }

    fn with(v: KbValues) -> Self {
        let f = Self {
            name: RwSignal::new(String::new()),
            description: RwSignal::new(String::new()),
            embed: RwSignal::new(String::new()),
            rerank: RwSignal::new(String::new()),
            vision: RwSignal::new(String::new()),
            chunk_tokens: RwSignal::new(String::new()),
            chunk_overlap: RwSignal::new(String::new()),
            mcp_visible: RwSignal::new(false),
            baseline: RwSignal::new(v.clone()),
        };
        f.write(&v);
        f
    }

    /// Put `v` into the fields; one already holding its value is left
    /// alone, so the field being typed in keeps its caret.
    fn write(&self, v: &KbValues) {
        fn put<T: Clone + PartialEq + Send + Sync + 'static>(sig: RwSignal<T>, v: &T) {
            if sig.with_untracked(|x| x != v) {
                sig.set(v.clone());
            }
        }
        put(self.name, &v.name);
        put(self.description, &v.description);
        put(self.embed, &v.embed);
        put(self.rerank, &v.rerank);
        put(self.vision, &v.vision);
        put(self.chunk_tokens, &v.chunk_tokens);
        put(self.chunk_overlap, &v.chunk_overlap);
        put(self.mcp_visible, &v.mcp_visible);
    }

    /// The values as they stand, taken when a save's body is built: what
    /// the save posts, and so what it rebases to.
    pub fn snapshot(&self) -> KbValues {
        KbValues {
            name: self.name.get_untracked(),
            description: self.description.get_untracked(),
            embed: self.embed.get_untracked(),
            rerank: self.rerank.get_untracked(),
            vision: self.vision.get_untracked(),
            chunk_tokens: self.chunk_tokens.get_untracked(),
            chunk_overlap: self.chunk_overlap.get_untracked(),
            mcp_visible: self.mcp_visible.get_untracked(),
        }
    }

    /// What was just saved (`sent`) is the baseline; whatever was typed
    /// after the body was built stays a change.
    pub fn rebase(&self, sent: KbValues) {
        self.baseline.set(sent);
    }

    /// The base as read again (`now`, with whatever the caller holds in
    /// place of a value the server has not taken yet): the baseline moves to
    /// it and every field the owner has not changed follows.
    pub fn merge(&self, now: KbValues) {
        let old = self.baseline.get_untracked();
        if old == now {
            return;
        }
        let typed = self.snapshot().follow(&old, &now);
        self.baseline.set(now);
        self.write(&typed);
    }

    /// Every field back to the baseline (Discard).
    pub fn discard(&self) {
        let b = self.baseline.get_untracked();
        self.write(&b);
    }

    /// What differs from the baseline. Tracked: call it in a memo.
    pub fn dirty(&self) -> KbDirty {
        let typed = KbValues {
            name: self.name.get(),
            description: self.description.get(),
            embed: self.embed.get(),
            rerank: self.rerank.get(),
            vision: self.vision.get(),
            chunk_tokens: self.chunk_tokens.get(),
            chunk_overlap: self.chunk_overlap.get(),
            mcp_visible: self.mcp_visible.get(),
        };
        self.baseline.with(|b| typed.dirty_against(b))
    }

    fn int_of(t: &str, label: &str) -> Result<i64, String> {
        t.trim()
            .parse::<i64>()
            .map_err(|_| format!("{label} must be a whole number (got '{}')", t.trim()))
    }

    /// Why the typed number is refused, or `None`. Tracked.
    pub fn int_error(sig: RwSignal<String>, label: &str) -> Option<String> {
        sig.with(|t| Self::int_of(t, label).err())
    }

    /// How many fields do not parse. Tracked.
    pub fn invalid(&self) -> usize {
        [
            Self::int_error(self.chunk_tokens, "Chunk size"),
            Self::int_error(self.chunk_overlap, "Chunk overlap"),
        ]
        .iter()
        .filter(|e| e.is_some())
        .count()
    }

    /// The request body. Every field is sent, so a create and an edit read
    /// the same and an emptied reranker / vision model clears it.
    pub fn body(&self) -> Result<Value, String> {
        Ok(json!({
            "name": self.name.get_untracked().trim(),
            "description": self.description.get_untracked().trim(),
            "embed_alias": self.embed.get_untracked().trim(),
            "rerank_alias": self.rerank.get_untracked().trim(),
            "vision_alias": self.vision.get_untracked().trim(),
            "chunk_tokens": Self::int_of(&self.chunk_tokens.get_untracked(), "Chunk size")?,
            "chunk_overlap": Self::int_of(&self.chunk_overlap.get_untracked(), "Chunk overlap")?,
            "mcp_visible": self.mcp_visible.get_untracked(),
        }))
    }
}

/// A model that says it cannot see is not offered for reading pages.
fn cannot_see(e: CatalogEntry) -> bool {
    e.vision == Some(false)
}

/// The fields, as three groups and a switch. `limit` is what the base's
/// embedding model really takes per input, once the base exists; a new base
/// only knows the catalog's context. `dirty` marks the changed rows.
#[component]
pub fn KbFields(
    form: KbForm,
    /// `(tokens, where it comes from)` for an existing base.
    #[prop(optional, into)]
    limit: Option<Signal<(Option<u64>, String)>>,
    #[prop(optional, into)] dirty: Option<Signal<KbDirty>>,
) -> impl IntoView {
    let catalog = use_model_catalog();
    let ctx_of = move || {
        let alias = form.embed.get();
        catalog
            .entries
            .with(|es| es.iter().find(|e| e.id == alias).and_then(|e| e.ctx))
    };
    let d =
        move |f: fn(&KbDirty) -> bool| Signal::derive(move || dirty.is_some_and(|d| f(&d.get())));
    let limit_note = move || {
        match limit {
        Some(l) => match l.get() {
            (Some(n), why) => format!(
                "The embedding model takes at most {n} tokens per input ({why}). A chunk is embedded with its heading path, so its size must stay within it; a larger value is refused, never clamped."
            ),
            (None, why) => format!(
                "The embedding model's per-input limit is {why}. The chunk size could not be checked against it."
            ),
        },
        None => match ctx_of() {
            Some(c) => format!(
                "This model's context is {c} tokens. A local embedder also takes at most its batch size per input (llama-server's default is 512); the exact limit is checked when the base is created, and a larger chunk size is refused with the number."
            ),
            None => "This model's context length is unknown to the gateway, so the chunk size cannot be checked in advance.".to_string(),
        },
    }
    };
    view! {
        <PrefGroup title="Knowledge base">
            <EntryRow title="Name" value=form.name placeholder="Taxes" dirty=d(|d| d.name)/>
            <EntryRow
                title="Description"
                sub="Models read it to pick a base"
                value=form.description
                placeholder="what is in it"
                wide=true
                dirty=d(|d| d.description)
            />
            <SwitchRow
                title="Available to API clients"
                sub="The kb__* tools on /mcp search it, subject to each key's tool scope"
                value=form.mcp_visible
                dirty=d(|d| d.mcp_visible)
            />
        </PrefGroup>
        <PrefGroup title="Models">
            <StackRow title="Embedding model" sub="Pinned when the base is made: its identity is probed" dirty=d(|d| d.embed) class="kb-picker">
                <ModelPicker value=form.embed tasks=&["embedding"] recent_key="knowledge.embed"/>
            </StackRow>
            <StackRow title="Reranker" sub="Optional" dirty=d(|d| d.rerank) class="kb-picker">
                <ModelPicker
                    value=form.rerank
                    tasks=&["rerank"]
                    empty_label="none — ranked by keyword + vector fusion"
                    recent_key="knowledge.rerank"
                />
            </StackRow>
            <StackRow title="Vision model" sub="Optional: reads PDF pages that have no text" dirty=d(|d| d.vision) class="kb-picker">
                <ModelPicker
                    value=form.vision
                    tasks=&["chat"]
                    empty_label="none — PDF pages without text are skipped and counted"
                    disallow=(Callback::new(cannot_see), "does not take images")
                    recent_key="knowledge.vision"
                />
            </StackRow>
        </PrefGroup>
        <PrefGroup title="Chunks" desc=Signal::derive(limit_note)>
            <NumberRow
                title="Chunk size"
                value=form.chunk_tokens
                unit="tokens"
                dirty=d(|d| d.chunk_tokens)
                error=Signal::derive(move || KbForm::int_error(form.chunk_tokens, "Chunk size"))
            />
            <NumberRow
                title="Chunk overlap"
                value=form.chunk_overlap
                unit="tokens"
                dirty=d(|d| d.chunk_overlap)
                error=Signal::derive(move || KbForm::int_error(form.chunk_overlap, "Chunk overlap"))
            />
        </PrefGroup>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_number_that_does_not_parse_is_said() {
        assert_eq!(KbForm::int_of(" 300 ", "Chunk size"), Ok(300));
        assert!(KbForm::int_of("3x", "Chunk size")
            .unwrap_err()
            .contains("whole number"));
        assert!(!differs("Taxes ", "Taxes"));
        assert!(differs("Taxes2", "Taxes"));
    }
}
