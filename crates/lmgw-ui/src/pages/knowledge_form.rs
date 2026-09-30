//! The fields of a knowledge base, shared by the New dialog and the Settings
//! tab: one definition of what a base is made of.

use leptos::prelude::*;
use serde_json::{json, Value};

use crate::catalog::{use_model_catalog, CatalogEntry};
use crate::widgets::ModelPicker;

use super::knowledge::Kb;

/// A base being typed. Numbers stay text until they are sent, so an empty or
/// half-typed one is a message, not a silent default.
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
}

impl KbForm {
    /// A new base: the backend's defaults (512 / 64, visible on /mcp).
    pub fn blank() -> Self {
        Self {
            name: RwSignal::new(String::new()),
            description: RwSignal::new(String::new()),
            embed: RwSignal::new(String::new()),
            rerank: RwSignal::new(String::new()),
            vision: RwSignal::new(String::new()),
            chunk_tokens: RwSignal::new("512".into()),
            chunk_overlap: RwSignal::new("64".into()),
            mcp_visible: RwSignal::new(true),
        }
    }

    pub fn of(kb: &Kb) -> Self {
        Self {
            name: RwSignal::new(kb.name.clone()),
            description: RwSignal::new(kb.description.clone()),
            embed: RwSignal::new(kb.embed_alias.clone()),
            rerank: RwSignal::new(kb.rerank_alias.clone()),
            vision: RwSignal::new(kb.vision_alias.clone()),
            chunk_tokens: RwSignal::new(kb.chunk_tokens.to_string()),
            chunk_overlap: RwSignal::new(kb.chunk_overlap.to_string()),
            mcp_visible: RwSignal::new(kb.mcp_visible),
        }
    }

    fn int(sig: RwSignal<String>, label: &str) -> Result<i64, String> {
        let t = sig.get_untracked();
        t.trim()
            .parse::<i64>()
            .map_err(|_| format!("{label} must be a whole number (got '{}')", t.trim()))
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
            "chunk_tokens": Self::int(self.chunk_tokens, "Chunk tokens")?,
            "chunk_overlap": Self::int(self.chunk_overlap, "Chunk overlap")?,
            "mcp_visible": self.mcp_visible.get_untracked(),
        }))
    }
}

/// A model that says it cannot see is not offered for reading pages.
fn cannot_see(e: CatalogEntry) -> bool {
    e.vision == Some(false)
}

/// The fields. `limit` is what the base's embedding model really takes per
/// input, once the base exists; a new base only knows the catalog's context.
#[component]
pub fn KbFields(
    form: KbForm,
    /// `(tokens, where it comes from)` for an existing base.
    #[prop(optional, into)]
    limit: Option<Signal<(Option<u64>, String)>>,
) -> impl IntoView {
    let catalog = use_model_catalog();
    let ctx_of = move || {
        let alias = form.embed.get();
        catalog
            .entries
            .with(|es| es.iter().find(|e| e.id == alias).and_then(|e| e.ctx))
    };
    view! {
        <div class="spec-grid">
            <div class="field">
                <label>"Name"</label>
                <input
                    class="input"
                    placeholder="Taxes"
                    prop:value=move || form.name.get()
                    on:input=move |ev| form.name.set(event_target_value(&ev))
                />
            </div>
            <div class="field">
                <label>"Description"</label>
                <input
                    class="input"
                    placeholder="what is in it — models read this to pick a base"
                    prop:value=move || form.description.get()
                    on:input=move |ev| form.description.set(event_target_value(&ev))
                />
            </div>
            <div class="field">
                <label>"Embedding model"</label>
                <ModelPicker value=form.embed tasks=&["embedding"] recent_key="knowledge.embed"/>
            </div>
            <div class="field">
                <label>"Reranker · optional"</label>
                <ModelPicker
                    value=form.rerank
                    tasks=&["rerank"]
                    empty_label="none — ranked by keyword + vector fusion"
                    recent_key="knowledge.rerank"
                />
            </div>
            <div class="field">
                <label>"Vision model · optional"</label>
                <ModelPicker
                    value=form.vision
                    tasks=&["chat"]
                    empty_label="none — PDF pages without text are skipped and counted"
                    disallow=(Callback::new(cannot_see), "does not take images")
                    recent_key="knowledge.vision"
                />
            </div>
        </div>
        <div class="spec-grid" style="margin-top:10px">
            <div class="field">
                <label>"Chunk tokens"</label>
                <input
                    class="input mono"
                    inputmode="numeric"
                    prop:value=move || form.chunk_tokens.get()
                    on:input=move |ev| form.chunk_tokens.set(event_target_value(&ev))
                />
            </div>
            <div class="field">
                <label>"Chunk overlap"</label>
                <input
                    class="input mono"
                    inputmode="numeric"
                    prop:value=move || form.chunk_overlap.get()
                    on:input=move |ev| form.chunk_overlap.set(event_target_value(&ev))
                />
            </div>
        </div>
        <div class="dim mini-note kb-limit">
            {move || match limit {
                Some(l) => {
                    match l.get() {
                        (Some(n), why) => {
                            format!(
                                "The embedding model takes at most {n} tokens per input ({why}). A chunk is embedded with its heading path, so chunk tokens must stay within it; a larger value is refused, never clamped.",
                            )
                        }
                        (None, why) => {
                            format!(
                                "The embedding model's per-input limit is {why}. Chunk tokens could not be checked against it.",
                            )
                        }
                    }
                }
                None => {
                    match ctx_of() {
                        Some(c) => {
                            format!(
                                "This model's context is {c} tokens. A local embedder also takes at most its batch size per input (llama-server's default is 512); the exact limit is checked when the base is created, and a larger chunk size is refused with the number.",
                            )
                        }
                        None => {
                            "This model's context length is unknown to the gateway, so chunk tokens cannot be checked in advance."
                                .to_string()
                        }
                    }
                }
            }}
        </div>
        <label class="row" style="gap:6px; margin-top:12px">
            <input
                type="checkbox"
                prop:checked=move || form.mcp_visible.get()
                on:change=move |ev| form.mcp_visible.set(event_target_checked(&ev))
            />
            "Available to API clients (/mcp kb__* tools, subject to key tool scope)"
        </label>
    }
}
