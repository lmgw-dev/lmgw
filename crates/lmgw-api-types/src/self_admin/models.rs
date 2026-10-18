//! `lmgw__models`

use serde::{Deserialize, Serialize};

/// `lmgw__models`: the model names the gateway can route, by source, and the
/// upstreams whose catalogs it cannot list without calling them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelList {
    /// By client-facing name.
    pub models: Vec<ModelEntry>,
    /// Enabled `expose_all` upstreams: they serve names the list cannot
    /// enumerate.
    pub passthrough_upstreams: Vec<PassthroughNote>,
}

/// One model name, with `kind` saying which source it comes from and so which
/// keys it has. Candidate aliases share the `alias` bucket of the tool's
/// `kind` argument, but each entry says `candidate_alias`.
///
/// No `Default`: a reader that finds no `kind` has no entry to invent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ModelEntry {
    /// A cloud alias: a name routed to one upstream's model.
    Alias(AliasEntry),
    /// A name routed to the first usable one of several local chat models.
    CandidateAlias(CandidateAliasEntry),
    /// A local llama-server chat model.
    Local(LocalEntry),
    /// A local embedding, rerank or decision model.
    Aux(AuxEntry),
    /// A local audio.cpp model.
    Audio(AudioEntry),
    /// A local stable-diffusion.cpp pipeline.
    Image(ImageEntry),
}

impl ModelEntry {
    /// The client-facing name, whichever the source.
    pub fn name(&self) -> &str {
        match self {
            Self::Alias(m) => &m.name,
            Self::CandidateAlias(m) => &m.name,
            Self::Local(m) => &m.name,
            Self::Aux(m) => &m.name,
            Self::Audio(m) => &m.name,
            Self::Image(m) => &m.name,
        }
    }
}

/// What `GET /v1/models` knows about a name, joined onto its entry by name.
/// Every key is absent when the gateway does not know it, never `null` and
/// never a guessed zero.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ModelFacts {
    /// The context window in tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u64>,
    /// One response's generation cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    /// The `capabilities` object of the name on `GET /v1/models`. Absent too
    /// when the model's weights could not be read; the notes then say why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<crate::capabilities::ModelCapabilities>,
    /// What the gateway noticed about the model, one sentence each; absent
    /// when there is none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// A cloud alias.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AliasEntry {
    pub name: String,
    pub enabled: bool,
    /// The upstream's name; `null` when the alias points at one that is gone.
    pub upstream: Option<String>,
    /// The model id sent to the upstream.
    pub upstream_model: String,
    #[serde(flatten)]
    pub facts: ModelFacts,
}

/// A candidate alias (candidate-aliases design §4.1).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CandidateAliasEntry {
    pub name: String,
    pub id: i64,
    pub enabled: bool,
    /// Local chat model ids, the primary first.
    pub candidates: Vec<String>,
    /// Background traffic is a GPU guest and never disturbs the owner.
    pub background: bool,
    /// `inherit | none | alias`.
    pub fallback_mode: String,
    /// The fallback alias, for mode `alias`; `null` otherwise.
    pub fallback: Option<String>,
    /// The facets the alias publishes, as last saved.
    pub enabled_facets: Vec<String>,
    /// The facets every usable candidate supports now.
    pub common_facets: Vec<String>,
    /// The candidates the gate may route to, in list order.
    pub routable_candidates: Vec<String>,
    /// What is wrong with a candidate or the fallback.
    pub problems: Vec<String>,
    pub advisories: Vec<String>,
    /// The alias fallback resolves and is not local.
    pub fallback_usable: bool,
    /// Requests deferred for the GPU in the last 24 hours.
    pub deferrals_24h: u64,
    #[serde(flatten)]
    pub facts: ModelFacts,
}

/// A local llama-server chat model.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LocalEntry {
    pub name: String,
    pub enabled: bool,
    /// Listed on `GET /v1/models`.
    pub public: bool,
    pub gguf_path: String,
    pub id: i64,
    #[serde(flatten)]
    pub facts: ModelFacts,
}

/// A local embedding, rerank or decision model.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AuxEntry {
    pub name: String,
    /// `embed | rerank | decision`.
    pub aux_kind: String,
    pub enabled: bool,
    pub id: i64,
    pub gguf_path: String,
    /// The pooling override; `null` for the model's own.
    pub pooling: Option<String>,
    #[serde(flatten)]
    pub facts: ModelFacts,
}

/// A local audio.cpp model.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct AudioEntry {
    pub name: String,
    pub enabled: bool,
    /// The audio.cpp `--task` (`tts`, `asr`, ...).
    pub task: String,
    /// The model-loader family.
    pub family: String,
    pub id: i64,
    #[serde(flatten)]
    pub facts: ModelFacts,
}

/// A local stable-diffusion.cpp pipeline: a set of files rather than one
/// path, so the entry names the loader and what the pipeline does.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ImageEntry {
    pub name: String,
    pub enabled: bool,
    pub id: i64,
    /// The pipelines it runs (`img_gen`, `vid_gen`), `img_gen` when the row
    /// names none.
    pub modes: Vec<String>,
    /// It takes reference images.
    pub edit: bool,
    /// What the row's `files` map names under `model`, else under
    /// `diffusion_model` (the stored value, usually a path); `null` when it
    /// names neither.
    pub loads: Option<serde_json::Value>,
    #[serde(flatten)]
    pub facts: ModelFacts,
}

/// An enabled `expose_all` upstream: its catalog is only known by calling it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PassthroughNote {
    pub upstream: String,
    /// Request `<prefix>/<model>`.
    pub prefix: String,
    /// How to reach its models, in a sentence.
    pub note: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `kind` names the entry's struct, and the facts sit beside its keys: a
    /// key the gateway does not know is not there, not `null`.
    #[test]
    fn an_entry_is_tagged_by_kind_with_its_facts_flattened_in() {
        let entry = ModelEntry::CandidateAlias(CandidateAliasEntry {
            name: "smart".into(),
            deferrals_24h: 0,
            facts: ModelFacts {
                context_length: Some(8192),
                ..Default::default()
            },
            ..Default::default()
        });
        let wire = serde_json::to_value(&entry).unwrap();
        assert_eq!(wire["kind"], "candidate_alias");
        assert_eq!(wire["context_length"], json!(8192));
        assert!(wire["context_length"].is_u64());
        assert_eq!(wire["deferrals_24h"], json!(0));
        assert!(wire["fallback"].is_null() && wire.get("fallback").is_some());
        for absent in ["max_output_tokens", "capabilities", "notes", "facts"] {
            assert!(wire.get(absent).is_none(), "{absent}: {wire}");
        }
        assert_eq!(serde_json::from_value::<ModelEntry>(wire).unwrap(), entry);
        assert_eq!(entry.name(), "smart");
    }

    /// Every kind's tag is its snake_case name.
    #[test]
    fn each_kind_reads_back_under_its_own_tag() {
        for (kind, entry) in [
            ("alias", ModelEntry::Alias(Default::default())),
            (
                "candidate_alias",
                ModelEntry::CandidateAlias(Default::default()),
            ),
            ("local", ModelEntry::Local(Default::default())),
            ("aux", ModelEntry::Aux(Default::default())),
            ("audio", ModelEntry::Audio(Default::default())),
            ("image", ModelEntry::Image(Default::default())),
        ] {
            let wire = serde_json::to_value(&entry).unwrap();
            assert_eq!(wire["kind"], kind);
            assert_eq!(serde_json::from_value::<ModelEntry>(wire).unwrap(), entry);
        }
    }

    /// An empty list is two empty arrays; an entry without `kind` is refused.
    #[test]
    fn the_list_and_its_entries() {
        let empty = serde_json::to_value(ModelList::default()).unwrap();
        assert_eq!(empty, json!({"models": [], "passthrough_upstreams": []}));
        assert!(serde_json::from_value::<ModelEntry>(json!({"name": "x"})).is_err());
    }
}
