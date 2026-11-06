//! Originals and their rank (find-a-model design D14, D15, D126): which of
//! the search's repos are a model rather than an upload of one, which kind
//! of row each would become, and their order. Every signal is a field of
//! the hub's live answer; no name is known in advance.

use std::collections::HashMap;

use crate::hf::search::HubModel;

use super::words::{matches, Match};

/// The kind of row an original would become, by the hub's own pipeline
/// names (D126): a fixed rule over the hub's taxonomy, not a model list.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Kind {
    Chat,
    Embed,
    Rerank,
    /// Any other pipeline (image, speech, …): D30's, pointed elsewhere.
    Other(String),
}

impl Kind {
    pub fn of(m: &HubModel) -> Self {
        match m.pipeline_tag.as_deref() {
            Some("feature-extraction" | "sentence-similarity") => Kind::Embed,
            Some("text-classification") => Kind::Rerank,
            None | Some("text-generation" | "image-text-to-text" | "any-to-any") => Kind::Chat,
            Some(other) => Kind::Other(other.to_string()),
        }
    }

    /// Whether Find a model sets this kind up: `model_get` creates a chat
    /// or an aux row for it (D15, ruled 2026-11-04). Any other kind is
    /// D30's, pointed elsewhere.
    pub fn sets_up(&self) -> bool {
        !matches!(self, Kind::Other(_))
    }

    /// The models directory class its files land in.
    pub fn target(&self) -> &'static str {
        match self {
            Kind::Embed | Kind::Rerank => "aux",
            _ => "chat",
        }
    }
}

/// The maker's own GGUF repo's name: the original's with a `-GGUF` (or
/// `_GGUF`, `.GGUF`) ending taken off.
pub fn strip_gguf(name: &str) -> &str {
    let lower = name.to_ascii_lowercase();
    for end in ["-gguf", "_gguf", ".gguf"] {
        if lower.ends_with(end) {
            return &name[..name.len() - end.len()];
        }
    }
    name
}

/// One original found by the search, with its match.
#[derive(Debug, Clone)]
pub struct Original {
    pub model: HubModel,
    pub matched: Match,
    pub kind: Kind,
    /// The maker's own GGUF repo, folded in as an upload (D14).
    pub makers_gguf: Option<HubModel>,
}

impl Original {
    pub fn id(&self) -> &str {
        &self.model.id
    }
}

/// What the search's answers hold: the originals by rank (D15) and the
/// repos that matched only some of the query's words, by how many (D13).
#[derive(Debug, Clone, Default)]
pub struct Ranked {
    pub originals: Vec<Original>,
    pub partial: Vec<(HubModel, Match)>,
}

/// The union of both searches' repos, the first answer's order kept.
pub fn union(all: Vec<HubModel>, gguf: Vec<HubModel>) -> Vec<HubModel> {
    let mut seen = std::collections::HashSet::new();
    all.into_iter()
        .chain(gguf)
        .filter(|m| seen.insert(m.id.to_ascii_lowercase()))
        .collect()
}

/// Whether `m` is an original (D14): it is, unless one of its base models
/// also matches the query fully and is either by another author or a
/// quantization of it.
fn is_original(m: &HubModel, query: &[String]) -> bool {
    let (relation, bases) = m.bases();
    !bases.iter().any(|b| {
        let (owner, name) = b.split_once('/').unwrap_or(("", b));
        matches(query, name).full()
            && (relation == Some("quantized") || !owner.eq_ignore_ascii_case(m.author()))
    })
}

/// D15's order across authors, highest first, then the id.
type RankKey = (
    std::cmp::Reverse<(bool, usize, usize, bool, u64, u64)>,
    String,
);

/// D15's order across authors: a kind Find a model sets up before any
/// other (ruled 2026-11-04), then the match, weights of its own, likes,
/// then downloads. Every original matches each query word (D13), so the
/// kind leads among them all.
fn rank_key(o: &Original) -> RankKey {
    (
        std::cmp::Reverse((
            o.kind.sets_up(),
            o.matched.exact,
            o.matched.loose,
            o.model.own_weights(),
            o.model.likes,
            o.model.downloads,
        )),
        o.model.id.clone(),
    )
}

/// The search's repos as originals by rank and partial matches (D13, D14,
/// D15).
pub fn rank(repos: Vec<HubModel>, query: &[String]) -> Ranked {
    let mut originals = Vec::new();
    let mut partial = Vec::new();
    for m in repos {
        let matched = matches(query, m.name());
        if !matched.full() {
            if matched.matched() > 0 {
                partial.push((m, matched));
            }
            continue;
        }
        if is_original(&m, query) {
            originals.push(Original {
                kind: Kind::of(&m),
                model: m,
                matched,
                makers_gguf: None,
            });
        }
    }
    // The maker's own GGUF (same author, the original's name plus `-GGUF`,
    // no weights of its own) is an upload of its original, even when the
    // hub lists no base model for it.
    let index: HashMap<String, usize> = originals
        .iter()
        .enumerate()
        .map(|(i, o)| (o.model.id.to_ascii_lowercase(), i))
        .collect();
    let mut folded: Vec<(usize, usize)> = Vec::new();
    for (i, o) in originals.iter().enumerate() {
        if o.model.own_weights() || !o.model.has_tag("gguf") {
            continue;
        }
        let twin =
            format!("{}/{}", o.model.author(), strip_gguf(o.model.name())).to_ascii_lowercase();
        if twin == o.model.id.to_ascii_lowercase() {
            continue;
        }
        if let Some(&t) = index.get(&twin) {
            folded.push((i, t));
        }
    }
    for &(i, t) in &folded {
        originals[t].makers_gguf = Some(originals[i].model.clone());
    }
    let gone: std::collections::HashSet<usize> = folded.iter().map(|(i, _)| *i).collect();
    let mut originals: Vec<Original> = originals
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !gone.contains(i))
        .map(|(_, o)| o)
        .collect();
    originals.sort_by_key(rank_key);
    partial.sort_by(|(a, am), (b, bm)| {
        (bm.matched(), b.likes, b.downloads, &a.id).cmp(&(
            am.matched(),
            a.likes,
            a.downloads,
            &b.id,
        ))
    });
    Ranked { originals, partial }
}

/// The walk's originals (D17, D126): the top original's author's, of its
/// kind and its match (the top is of a kind Find a model sets up whenever
/// any original is, D15), with a chat model before a base one within that
/// author (D15): one the hub tags `conversational`, or a finetune of an
/// original in the same set.
pub fn walk_set(ranked: &[Original]) -> Vec<&Original> {
    let Some(top) = ranked.first() else {
        return Vec::new();
    };
    let author = top.model.author().to_ascii_lowercase();
    let same: Vec<&Original> = ranked
        .iter()
        .filter(|o| {
            o.model.author().eq_ignore_ascii_case(&author)
                && o.kind == top.kind
                && (o.matched.exact, o.matched.loose) == (top.matched.exact, top.matched.loose)
        })
        .collect();
    let ids: std::collections::HashSet<String> = same
        .iter()
        .map(|o| o.model.id.to_ascii_lowercase())
        .collect();
    let chat_signal = |o: &Original| -> bool {
        if top.kind != Kind::Chat {
            return true;
        }
        if o.model.has_tag("conversational") {
            return true;
        }
        let (relation, bases) = o.model.bases();
        matches!(relation, Some("finetune" | "adapter" | "merge"))
            && bases.iter().any(|b| ids.contains(&b.to_ascii_lowercase()))
    };
    let mut same = same;
    same.sort_by_key(|o| {
        (
            std::cmp::Reverse((
                o.matched.exact,
                o.matched.loose,
                o.model.own_weights(),
                chat_signal(o),
                o.model.likes,
                o.model.downloads,
            )),
            o.model.id.clone(),
        )
    });
    same
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::find::words::words;
    use crate::hf::search::{BaseModel, BaseModels, Safetensors};

    fn repo(id: &str, likes: u64) -> HubModel {
        HubModel {
            id: id.into(),
            likes,
            gated: serde_json::Value::Bool(false),
            safetensors: Some(Safetensors {
                total: Some(1_000_000_000),
            }),
            ..HubModel::default()
        }
    }

    fn based(mut m: HubModel, relation: &str, base: &str) -> HubModel {
        m.base_models = Some(BaseModels {
            relation: Some(relation.into()),
            models: vec![BaseModel { id: base.into() }],
        });
        m
    }

    #[test]
    fn an_upload_of_a_matching_model_is_no_original() {
        let q = words("demo 2");
        let ranked = rank(
            vec![
                based(
                    repo("other-demo/Demo-2-abliterated-GGUF", 900),
                    "quantized",
                    "maker-demo/Demo-2-8B",
                ),
                based(
                    repo("maker-demo/Demo-2-8B-FP8", 50),
                    "quantized",
                    "maker-demo/Demo-2-8B",
                ),
                based(
                    repo("maker-demo/Demo-2-8B-it", 400),
                    "finetune",
                    "maker-demo/Demo-2-8B",
                ),
                repo("maker-demo/Demo-2-8B", 500),
                repo("someone-demo/Demo-3-2B", 9000),
                repo("someone-demo/Other-2", 9000),
            ],
            &q,
        );
        let ids: Vec<&str> = ranked.originals.iter().map(|o| o.id()).collect();
        // "2" only starts "2b" in Demo-3-2B: loose, so it ranks below the
        // exact ones whatever its likes.
        assert_eq!(
            ids,
            [
                "maker-demo/Demo-2-8B",
                "maker-demo/Demo-2-8B-it",
                "someone-demo/Demo-3-2B"
            ]
        );
        // "demo" is missing from Other-2: a partial line.
        assert_eq!(ranked.partial.len(), 1);
        assert_eq!(ranked.partial[0].0.id, "someone-demo/Other-2");
        // Within the author, the finetune of its own base leads.
        let walk: Vec<&str> = walk_set(&ranked.originals).iter().map(|o| o.id()).collect();
        assert_eq!(walk, ["maker-demo/Demo-2-8B-it", "maker-demo/Demo-2-8B"]);
    }

    #[test]
    fn the_makers_own_gguf_folds_into_its_original() {
        let q = words("demo embed");
        let mut gguf = repo("maker-demo/Demo-Embed-0.6B-GGUF", 30);
        gguf.safetensors = None;
        gguf.tags = vec!["gguf".into()];
        let ranked = rank(vec![gguf, repo("maker-demo/Demo-Embed-0.6B", 100)], &q);
        assert_eq!(ranked.originals.len(), 1);
        assert_eq!(
            ranked.originals[0]
                .makers_gguf
                .as_ref()
                .map(|m| m.id.as_str()),
            Some("maker-demo/Demo-Embed-0.6B-GGUF")
        );
    }

    fn piped(mut m: HubModel, pipeline: &str) -> HubModel {
        m.pipeline_tag = Some(pipeline.into());
        m
    }

    #[test]
    fn a_kind_find_a_model_sets_up_comes_before_another_kind() {
        // The owner's ruling of 2026-11-04 on "qwen3 1.7b": both match every
        // word exactly, the speech model has more likes, the chat model leads.
        let q = words("demo 1.7b");
        let ranked = rank(
            vec![
                piped(
                    repo("maker-demo/Demo-Speech-12Hz-1.7B", 2000),
                    "text-to-speech",
                ),
                piped(repo("maker-demo/Demo-1.7B", 600), "text-generation"),
                piped(repo("other-demo/Demo-Vision-1.7B", 900), "image-to-text"),
                // A loose match of a kind it sets up still leads the other kinds.
                piped(repo("fan-demo/Demo-1.7Bx", 1), "feature-extraction"),
                repo("someone-demo/Something-Else-1.7B", 9000),
            ],
            &q,
        );
        let ids: Vec<&str> = ranked.originals.iter().map(|o| o.id()).collect();
        assert_eq!(
            ids,
            [
                "maker-demo/Demo-1.7B",
                "fan-demo/Demo-1.7Bx",
                "maker-demo/Demo-Speech-12Hz-1.7B",
                "other-demo/Demo-Vision-1.7B",
            ]
        );
        // Partial matches keep their order: by words matched, then likes.
        assert_eq!(ranked.partial.len(), 1);
        assert_eq!(ranked.partial[0].0.id, "someone-demo/Something-Else-1.7B");
        let walk: Vec<&str> = walk_set(&ranked.originals).iter().map(|o| o.id()).collect();
        assert_eq!(walk, ["maker-demo/Demo-1.7B"]);
        // With only other kinds, the top is another kind (D30's answer); the
        // finder's answer for it is tested in `it/hf_find/regression.rs`.
        let only = rank(
            vec![piped(
                repo("maker-demo/Demo-Speech-1.7B", 5),
                "text-to-speech",
            )],
            &q,
        );
        assert_eq!(only.originals.len(), 1);
        assert!(!only.originals[0].kind.sets_up());
    }

    #[test]
    fn weights_of_its_own_come_before_likes() {
        let q = words("demo");
        let mut only_gguf = repo("fan-demo/Demo-Tuned-GGUF", 5000);
        only_gguf.safetensors = None;
        only_gguf.tags = vec!["gguf".into()];
        let ranked = rank(vec![only_gguf, repo("maker-demo/Demo-7B", 10)], &q);
        assert_eq!(ranked.originals[0].id(), "maker-demo/Demo-7B");
    }
}
