//! Where an OpenAI-shaped catalog request goes (decision-models design §6.2).
//!
//! OpenRouter's default `GET /models` lists only the models whose output is
//! text. Jev and every other decision model, the embedders, rerankers,
//! speech, transcription, image and video models appear only with
//! `?output_modalities=all` (Appendix B.3). So the catalog request of an
//! `openai` row of kind `generic` carries that query unless its owner turned
//! `catalog_all_modalities` off. OpenAI, Kilo's gateway and llama-server
//! answered the same list with and without it; Gemini's OpenAI-compatible
//! endpoint refuses it with a 400, which is why the option exists (live
//! check, 2026-10-14, §6.2). A `llama_cpp` row lists every model it serves
//! anyway, so its request stays bare.

use crate::config::{Protocol, Snapshot, Upstream, UpstreamKind};

/// The query that asks a catalog for every output modality.
pub const ALL_MODALITIES: &str = "output_modalities=all";

/// Whether `u`'s catalog request asks for every output modality: an `openai`
/// row of kind `generic` whose `catalog_all_modalities` is on. An `openai`
/// row of another kind (an audio.cpp server, say) lists its own models and
/// was never asked, so it stays bare.
pub fn asks_all_modalities(snap: &Snapshot, u: &Upstream) -> bool {
    u.protocol == Protocol::Openai
        && u.kind == UpstreamKind::Generic
        && !snap.catalog_bare.contains(&u.id)
}

/// The `GET` an upstream that lists its models the OpenAI way answers:
/// `{base}/models`, with [`ALL_MODALITIES`] when [`asks_all_modalities`].
pub fn openai_models_url(base: &str, asks_all: bool) -> String {
    if asks_all {
        format!("{base}/models?{ALL_MODALITIES}")
    } else {
        format!("{base}/models")
    }
}

/// What a failed catalog request adds to its error when it asked for every
/// output modality and the upstream refused the request as malformed (400
/// or 422): the option that sends it, so the fix is named rather than left
/// to a guess. Any other failure gets nothing: an auth error or an outage
/// has nothing to do with the query.
pub fn refusal_hint(asked_all: bool, status: u16) -> &'static str {
    if asked_all && matches!(status, 400 | 422) {
        "; the request asked for every output modality (output_modalities=all), which this \
         upstream may refuse: switch off its catalog_all_modalities (Upstreams, \"list every \
         output modality\")"
    } else {
        ""
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream(id: i64, protocol: Protocol) -> Upstream {
        let snap = Snapshot::default();
        let mut u = snap.aux_upstream();
        u.id = id;
        u.protocol = protocol;
        u.kind = UpstreamKind::Generic;
        u
    }

    #[test]
    fn only_an_openai_row_with_the_option_on_asks_for_every_modality() {
        let mut snap = Snapshot::default();
        assert!(asks_all_modalities(&snap, &upstream(5, Protocol::Openai)));
        for p in [Protocol::LlamaCpp, Protocol::Anthropic, Protocol::Gemini] {
            assert!(!asks_all_modalities(&snap, &upstream(5, p)), "{p:?}");
        }
        snap.catalog_bare.insert(5);
        assert!(!asks_all_modalities(&snap, &upstream(5, Protocol::Openai)));
        assert!(asks_all_modalities(&snap, &upstream(6, Protocol::Openai)));
        for kind in [
            UpstreamKind::AudioCpp,
            UpstreamKind::SdCpp,
            UpstreamKind::LlamaServer,
        ] {
            let mut u = upstream(6, Protocol::Openai);
            u.kind = kind;
            assert!(!asks_all_modalities(&snap, &u), "{kind:?}");
        }
    }

    #[test]
    fn the_url_carries_the_query_only_when_asked() {
        assert_eq!(
            openai_models_url("https://openrouter.ai/api/v1", true),
            "https://openrouter.ai/api/v1/models?output_modalities=all"
        );
        assert_eq!(
            openai_models_url("http://127.0.0.1:9292/v1", false),
            "http://127.0.0.1:9292/v1/models"
        );
    }

    #[test]
    fn the_hint_names_the_option_only_for_a_malformed_request_that_asked() {
        assert!(refusal_hint(true, 400).contains("catalog_all_modalities"));
        assert!(refusal_hint(true, 422).contains("catalog_all_modalities"));
        assert_eq!(refusal_hint(true, 401), "");
        assert_eq!(refusal_hint(true, 503), "");
        assert_eq!(refusal_hint(false, 400), "");
    }
}
