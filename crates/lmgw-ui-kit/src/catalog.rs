//! The gateway's model catalog (UX plan §2 #12): `/v1/models`, fetched once
//! the session is open and shared by every picker, instead of each page
//! fetching and parsing its own copy.
//!
//! The entries are what a client of the gateway sees — ids, where they are
//! served from, what they are for (`capabilities.task`), context, price —
//! flattened into one struct so a picker can filter, group and badge them
//! without re-reading JSON. Unknown facts stay unknown (`None`/`false`):
//! a model whose task the gateway could not derive is not assumed to be
//! anything.

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::Value;

use crate::fmt::per_mtok;

/// The local runtimes, as `owned_by` names them, with their group labels —
/// in the order pickers list them, before the upstreams.
pub const LOCAL_SOURCES: [(&str, &str); 4] = [
    ("llama-server", "Local llama.cpp"),
    ("llama-aux", "Aux"),
    ("audiocpp", "audio.cpp"),
    ("sdcpp", "sd.cpp"),
];

/// How long a fetched catalog is trusted before a picker opening refreshes it.
pub const FRESH_SECS: f64 = 60.0;

#[derive(Clone, Debug, PartialEq)]
pub struct CatalogEntry {
    pub id: String,
    /// `owned_by`: a local runtime (`llama-server`, …) or the upstream's name.
    pub source: String,
    /// The group a picker files it under ("Local llama.cpp", "kilo-gw").
    pub group_label: String,
    /// The model's maker where the id names one (`kilo/google/gemma-…`).
    pub vendor: Option<String>,
    /// `capabilities.task`: chat, embedding, rerank, tts, asr,
    /// image_generation, image_edit.
    pub task: Option<String>,
    pub ctx: Option<u64>,
    /// Per million tokens.
    pub price_in: Option<f64>,
    pub price_out: Option<f64>,
    /// A router's published price is negative: "depends on where the request
    /// goes". Not a price — the prices above stay unknown and this says why.
    pub price_varies: bool,
    /// `Some(true)`/`Some(false)` when the catalog states it, `None` when it
    /// does not — an unstated vision flag is not "no" (chat-archive-pin-
    /// attachments §3): a model this is unknown for is still sent images,
    /// same as the gateway's own send-time rule.
    pub vision: Option<bool>,
    pub tools: bool,
    pub reasoning: bool,
    /// `capabilities.reasoning` in full, when stated — what the Chat page's
    /// reasoning overrides offer and say about the model.
    pub reasoning_facts: Option<ReasoningFacts>,
    pub local: bool,
}

/// What a model's `capabilities.reasoning` states: how a request controls
/// its thinking, and what it does when a request sets nothing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReasoningFacts {
    /// `fixed` (nothing to change per request), `toggle` (on/off), `levels`
    /// (an effort per request).
    pub kind: String,
    /// On or off by default, when a source says.
    pub enabled: Option<bool>,
    /// The accepted efforts, least to most; only for `levels`.
    pub levels: Vec<String>,
    /// The effort in force when a request sets none.
    pub default: Option<String>,
    /// Whether a request can switch thinking off entirely.
    pub can_disable: Option<bool>,
    /// The configured default thinking-token budget.
    pub budget_tokens: Option<i64>,
}

impl ReasoningFacts {
    fn from_v1(r: &Value) -> Self {
        let text = |k: &str| r.get(k).and_then(Value::as_str).map(str::to_string);
        Self {
            kind: text("kind").unwrap_or_default(),
            enabled: r.get("enabled").and_then(Value::as_bool),
            levels: r
                .get("levels")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            default: text("default"),
            can_disable: r.get("can_disable").and_then(Value::as_bool),
            budget_tokens: r.get("budget_tokens").and_then(Value::as_i64),
        }
    }
}

impl CatalogEntry {
    /// One `/v1/models` `data[]` element.
    pub fn from_v1(m: &Value) -> Option<Self> {
        let id = m.get("id")?.as_str()?.to_string();
        let source = m
            .get("owned_by")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let local_label = LOCAL_SOURCES
            .iter()
            .find(|(s, _)| *s == source)
            .map(|(_, l)| l.to_string());
        let caps = m.get("capabilities");
        let cap = |k: &str| caps.and_then(|c| c.get(k));
        let task = cap("task").and_then(Value::as_str).map(str::to_string);
        // `capabilities.vision` states it when the gateway knows; otherwise
        // `input_modalities` (also stated or not) is the fallback. Neither
        // present is unknown, not "no".
        let vision = match cap("vision").and_then(Value::as_bool) {
            Some(v) => Some(v),
            None => cap("input_modalities")
                .and_then(Value::as_array)
                .map(|a| a.iter().any(|x| x.as_str() == Some("image"))),
        };
        let tools = cap("tool_calls")
            .and_then(|t| t.get("kind"))
            .and_then(Value::as_str)
            .is_some_and(|k| k != "none");
        // A model that can think but whose thinking is fixed off is not a
        // reasoning model to the one picking it.
        let reasoning = cap("reasoning").is_some_and(|r| {
            let kind = r.get("kind").and_then(Value::as_str).unwrap_or("");
            let enabled = r.get("enabled").and_then(Value::as_bool);
            kind != "fixed" || enabled == Some(true)
        });
        let published = |k: &str| {
            m.get("pricing")
                .and_then(|p| p.get(k))
                .and_then(|v| match v {
                    Value::String(s) => per_mtok(s),
                    Value::Number(n) => n.as_f64().map(|x| x * 1e6),
                    _ => None,
                })
        };
        // A negative price is a router's "varies", the same rule as
        // `from_upstream` — never "$-1000000" (review code:C8).
        let price = |k: &str| published(k).filter(|v| *v >= 0.0);
        let price_varies = ["prompt", "completion"]
            .iter()
            .any(|k| published(k).is_some_and(|v| v < 0.0));
        let parts: Vec<&str> = id.split('/').collect();
        let vendor = (parts.len() >= 3).then(|| parts[1].to_string());
        Some(Self {
            group_label: local_label.clone().unwrap_or_else(|| source.clone()),
            local: local_label.is_some(),
            vendor,
            task,
            ctx: m.get("context_length").and_then(Value::as_u64),
            price_in: price("prompt"),
            price_out: price("completion"),
            price_varies,
            vision,
            tools,
            reasoning,
            reasoning_facts: cap("reasoning").map(ReasoningFacts::from_v1),
            source,
            id,
        })
    }

    /// One entry of an upstream's own catalog (the admin plane's upstream-models route), for
    /// a picker over that upstream alone: the id is bare, as the upstream
    /// names it — what an alias maps to — and the whole list is one group
    /// under the upstream's name. A negative published price is a router's
    /// "depends on where it goes", not a price, so it stays unknown.
    pub fn from_upstream(upstream: &str, e: &lmgw_api_types::UpstreamModelEntry) -> Self {
        let published = |p: &Option<String>| p.as_deref().and_then(per_mtok);
        let per_m = |p: &Option<String>| published(p).filter(|v| *v >= 0.0);
        let price_varies = [&e.price_prompt, &e.price_completion]
            .into_iter()
            .any(|p| published(p).is_some_and(|v| v < 0.0));
        let reasoning = match e.reasoning.as_deref() {
            Some("fixed") => e.reasoning_enabled == Some(true),
            Some(_) => true,
            None => false,
        };
        Self {
            id: e.id.clone(),
            source: upstream.to_string(),
            group_label: upstream.to_string(),
            vendor: e
                .id
                .split_once('/')
                .map(|(v, _)| v.to_string())
                .filter(|v| !v.is_empty()),
            task: e.task.clone(),
            ctx: e.context_length,
            price_in: per_m(&e.price_prompt),
            price_out: per_m(&e.price_completion),
            price_varies,
            // Unpublished modalities are unknown, not "no vision".
            vision: e
                .input_modalities
                .as_ref()
                .map(|m| m.iter().any(|x| x == "image")),
            tools: e.tools == Some(true),
            reasoning,
            reasoning_facts: None,
            local: false,
        }
    }

    /// Where its group sorts: the local runtimes in their fixed order, then
    /// the upstreams (by name, as the tie-break of the caller's sort).
    pub fn group_rank(&self) -> usize {
        LOCAL_SOURCES
            .iter()
            .position(|(s, _)| *s == self.source)
            .unwrap_or(LOCAL_SOURCES.len())
    }

    /// The id split before its last `/`: `("kilo/google/", "gemma-3-12b-it")`,
    /// so a list can dim the prefix every model of an upstream shares.
    pub fn split_name(&self) -> (&str, &str) {
        match self.id.rfind('/') {
            Some(i) => self.id.split_at(i + 1),
            None => ("", self.id.as_str()),
        }
    }

    /// Every (lowercased) filter word is in the id, the source, the group or
    /// the vendor.
    pub fn matches(&self, words: &[String]) -> bool {
        if words.is_empty() {
            return true;
        }
        let hay = format!(
            "{} {} {} {}",
            self.id,
            self.source,
            self.group_label,
            self.vendor.as_deref().unwrap_or("")
        )
        .to_lowercase();
        words.iter().all(|w| hay.contains(w.as_str()))
    }
}

/// The parsed, sorted catalog: grouped by source (locals first), by id
/// within a group.
pub fn parse_catalog(v: &Value) -> Vec<CatalogEntry> {
    let mut list: Vec<CatalogEntry> = v
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(CatalogEntry::from_v1)
        .collect();
    list.sort_by(|a, b| {
        (a.group_rank(), &a.group_label, &a.id).cmp(&(b.group_rank(), &b.group_label, &b.id))
    });
    list
}

/// The shared catalog (in context via [`provide_model_catalog`]).
#[derive(Clone, Copy)]
pub struct ModelCatalog {
    pub entries: RwSignal<Vec<CatalogEntry>>,
    /// The last fetch's failure; the entries keep what the one before got.
    pub error: RwSignal<Option<String>>,
    pub loading: RwSignal<bool>,
    /// When the entries were fetched (ms since the epoch; 0 = never).
    fetched_at: RwSignal<f64>,
    /// Which fetch is the latest: an older one landing late is dropped.
    generation: StoredValue<u64>,
}

impl ModelCatalog {
    /// Fetch `/v1/models` again. Every model, alias, upstream and visibility
    /// operation should call this once it succeeds, so pickers never offer a
    /// model that just went away.
    pub fn refresh(&self) {
        let this = *self;
        let gen = this.generation.get_value() + 1;
        this.generation.set_value(gen);
        this.loading.set(true);
        spawn_local(async move {
            let res = crate::http::get::<Value>("/v1/models").await;
            if this.generation.try_get_value() != Some(gen) {
                return;
            }
            this.loading.set(false);
            match res {
                Ok(v) => {
                    this.entries.set(parse_catalog(&v));
                    this.error.set(None);
                    this.fetched_at.set(js_sys::Date::now());
                }
                Err(e) => this.error.set(Some(e.to_string())),
            }
        });
    }

    /// [`Self::refresh`] unless the entries are younger than `secs`.
    pub fn refresh_if_older(&self, secs: f64) {
        let age = js_sys::Date::now() - self.fetched_at.get_untracked();
        if !self.loading.get_untracked() && age > secs * 1000.0 {
            self.refresh();
        }
    }
}

/// Install the catalog and fetch it whenever the session opens (on load, and
/// again after a login — the gate swaps the pages, and a catalog fetched
/// while locked is a 401). `locked` is the host's session gate. Call once, in `App`.
pub fn provide_model_catalog(locked: Signal<bool>) {
    let catalog = ModelCatalog {
        entries: RwSignal::new(Vec::new()),
        error: RwSignal::new(None),
        loading: RwSignal::new(false),
        fetched_at: RwSignal::new(0.0),
        generation: StoredValue::new(0),
    };
    provide_context(catalog);
    Effect::new(move |prev: Option<bool>| {
        let now = locked.get();
        if !now && prev != Some(false) {
            catalog.refresh();
        }
        now
    });
}

pub fn use_model_catalog() -> ModelCatalog {
    expect_context::<ModelCatalog>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Value {
        json!({"data": [
            {"id": "kilo/google/gemma-3-12b-it", "owned_by": "kilo-gw", "context_length": 131072,
             "pricing": {"prompt": "0.0000008", "completion": "0.0000016"},
             "capabilities": {"task": "chat", "vision": true,
                              "tool_calls": {"kind": "native"},
                              "reasoning": {"kind": "toggle"}}},
            {"id": "gemma4-12b", "owned_by": "llama-server", "context_length": 40000,
             "pricing": {"prompt": "0", "completion": "0"},
             "capabilities": {"task": "chat", "tool_calls": {"kind": "none"},
                              "reasoning": {"kind": "fixed", "enabled": false}}},
            {"id": "embed/Qwen3-Embedding-0.6B", "owned_by": "llama-aux",
             "capabilities": {"task": "embedding"}},
            {"id": "devstral-small-2-24b", "owned_by": "llama-server", "context_length": 40000},
            {"id": "aistudio/gemini-pro", "owned_by": "aistudio"}
        ]})
    }

    #[test]
    fn an_entry_carries_what_the_catalog_states() {
        let list = parse_catalog(&sample());
        let kilo = list.iter().find(|e| e.source == "kilo-gw").unwrap();
        assert_eq!(kilo.vendor.as_deref(), Some("google"));
        assert_eq!(kilo.task.as_deref(), Some("chat"));
        assert_eq!(kilo.ctx, Some(131072));
        assert_eq!(kilo.price_in, Some(0.8));
        assert_eq!(kilo.price_out, Some(1.6));
        assert_eq!(kilo.vision, Some(true));
        assert!(kilo.tools && kilo.reasoning && !kilo.local);
        assert_eq!(
            kilo.reasoning_facts.as_ref().map(|r| r.kind.as_str()),
            Some("toggle")
        );
        assert_eq!(kilo.split_name(), ("kilo/google/", "gemma-3-12b-it"));

        let local = list.iter().find(|e| e.id == "gemma4-12b").unwrap();
        assert!(local.local && !local.tools && !local.reasoning);
        assert_eq!(local.group_label, "Local llama.cpp");
        assert_eq!(local.split_name(), ("", "gemma4-12b"));
    }

    #[test]
    fn unknown_facts_stay_unknown() {
        let list = parse_catalog(&sample());
        let dev = list
            .iter()
            .find(|e| e.id == "devstral-small-2-24b")
            .unwrap();
        assert_eq!(dev.task, None);
        // No capabilities block at all: vision is unknown, not "no".
        assert_eq!(dev.vision, None);
        assert!(!dev.tools && !dev.reasoning);
        let gem = list.iter().find(|e| e.id == "aistudio/gemini-pro").unwrap();
        assert_eq!((gem.price_in, gem.vendor.as_deref()), (None, None));
    }

    #[test]
    fn locals_sort_first_in_runtime_order_then_upstreams() {
        let ids: Vec<String> = parse_catalog(&sample()).into_iter().map(|e| e.id).collect();
        assert_eq!(
            ids,
            [
                "devstral-small-2-24b",
                "gemma4-12b",
                "embed/Qwen3-Embedding-0.6B",
                "aistudio/gemini-pro",
                "kilo/google/gemma-3-12b-it",
            ]
        );
    }

    #[test]
    fn an_upstream_entry_keeps_its_bare_id_and_what_it_states() {
        let e = lmgw_api_types::UpstreamModelEntry {
            id: "qwen/qwen3-coder".into(),
            context_length: Some(262_144),
            price_prompt: Some("0.0000003".into()),
            price_completion: Some("-1".into()),
            input_modalities: Some(vec!["text".into(), "image".into()]),
            task: Some("chat".into()),
            tools: Some(true),
            reasoning: Some("fixed".into()),
            reasoning_enabled: Some(false),
            ..Default::default()
        };
        let c = CatalogEntry::from_upstream("kilo-gw", &e);
        assert_eq!(c.id, "qwen/qwen3-coder");
        assert_eq!(
            (c.source.as_str(), c.group_label.as_str()),
            ("kilo-gw", "kilo-gw")
        );
        assert_eq!(c.vendor.as_deref(), Some("qwen"));
        assert_eq!((c.price_in, c.price_out), (Some(0.3), None));
        assert!(c.price_varies);
        assert_eq!(c.vision, Some(true));
        assert!(c.tools && !c.reasoning && !c.local);
        assert_eq!(c.split_name(), ("qwen/", "qwen3-coder"));

        let bare = CatalogEntry::from_upstream("aistudio", &Default::default());
        assert_eq!(bare.vendor, None);
        // No published input_modalities: unknown, not "no".
        assert_eq!(bare.vision, None);
    }

    #[test]
    fn a_router_price_is_not_a_price() {
        let e = CatalogEntry::from_v1(&json!({
            "id": "kilo/kilo-auto/balanced", "owned_by": "kilo-gw",
            "pricing": {"prompt": "-1", "completion": "-1"}
        }))
        .unwrap();
        assert_eq!((e.price_in, e.price_out), (None, None));
        assert!(e.price_varies);
        let priced = parse_catalog(&sample());
        assert!(priced.iter().all(|e| !e.price_varies));
    }

    #[test]
    fn every_word_has_to_match_somewhere() {
        let list = parse_catalog(&sample());
        let words = crate::widgets::filter_words("gem 12");
        let hits: Vec<&str> = list
            .iter()
            .filter(|e| e.matches(&words))
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(hits, ["gemma4-12b", "kilo/google/gemma-3-12b-it"]);
        // the source and the vendor count too
        assert!(list[4].matches(&crate::widgets::filter_words("google kilo")));
        assert!(list[1].matches(&crate::widgets::filter_words("llama.cpp")));
    }
}
