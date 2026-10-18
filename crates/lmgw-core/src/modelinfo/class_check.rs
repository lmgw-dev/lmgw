//! What a chat row on a GGUF whose header classifies as aux
//! ([`super::class_for`]) is told: by `local_model_check`, and by a failed
//! load test's hint.

use crate::config::AuxKind;
use crate::gguf::{self, ModelSummary};
use crate::runtime::argv::{canonical_key, is_opt};

use super::class_for;
use super::decision::Shape;

/// What a chat row on a GGUF whose header classifies as aux ([`class_for`])
/// is told — `local_model_check`'s finding and a failed load test's hint:
/// the class and kind that serve the file, the header key that says so, and
/// why a chat row is the wrong home for it. `subject` names the file in the
/// sentence. `None` for a file the chat class serves.
///
/// A decision model is named whatever its type, because a chat row never
/// answers `/v1/systemone`; whether it can generate depends on the type (a
/// label-logit type's context still can), so the sentence does not claim it
/// cannot.
pub(crate) fn aux_file_on_chat_row(subject: &str, s: &ModelSummary) -> Option<String> {
    let ("aux", Some(kind)) = class_for(s) else {
        return None;
    };
    let pooling = s.pooling_type.and_then(gguf::pooling_name);
    let (declares, why) = match kind {
        AuxKind::Decision => (
            format!(
                "declares decision.type '{}'",
                s.decision_type.as_deref().unwrap_or_default()
            ),
            "it answers typed questions on /v1/systemone, which no chat row serves, and the \
             decision types that read embeddings run llama-server in embedding mode, where it \
             cannot generate",
        ),
        AuxKind::Rerank => (
            if s.has_classifier_head {
                "carries a classifier head (cls.output)".to_string()
            } else {
                "declares pooling_type 'rank'".to_string()
            },
            "it scores query/document pairs on /v1/rerank and has no LM head to generate with",
        ),
        AuxKind::Embed => (
            match pooling {
                Some(p) => format!("declares pooling_type '{p}'"),
                None => format!(
                    "is a {} encoder",
                    s.architecture.as_deref().unwrap_or_default()
                ),
            },
            "it answers /v1/embeddings and has no LM head to generate with",
        ),
    };
    Some(format!(
        "{subject} {declares}: the aux class serves it, as kind={} — {why}. Serve it as an aux \
         model instead: lmgw__local_model_plan with target=aux, then lmgw__aux_model_set (the \
         file belongs in the aux models dir)",
        kind.as_str()
    ))
}

/// Whether a chat row on the aux file `s`, with its freeform `args`, still
/// loads and serves something — which makes [`aux_file_on_chat_row`]'s
/// finding an advisory in `local_model_check` rather than a problem:
///
/// - an embedding or rerank file on a row whose args carry `--embedding(s)`
///   or `--reranking`: the old chat-row workaround, which loads and answers
///   `/v1/embeddings` or `/v1/rerank` (their guards check aux rows only);
/// - a label-logit decision file (lev, OpenJev: causal, no decision head),
///   whose server keeps a normal context and can still generate — unless
///   the args carry `--reranking`, which crashes it at load (decision-models
///   design A.10).
///
/// Anything else cannot serve a chat row: an encoder without its kind flag
/// has no LM head to generate with, and a non-causal or head-carrying
/// decision model (Laya, Kev, Clef) runs llama-server in embedding mode,
/// where chat answers 500 (§1.4). `true` for a file the chat class serves.
pub(crate) fn chat_row_still_serves(s: &ModelSummary, args: &[String]) -> bool {
    let has = |key: &str| {
        args.iter().filter(|a| is_opt(a)).any(|a| {
            let flag = a.trim_start_matches('-');
            canonical_key(flag.split('=').next().unwrap_or(flag)) == key
        })
    };
    match class_for(s) {
        ("aux", Some(AuxKind::Embed | AuxKind::Rerank)) => has("embeddings") || has("reranking"),
        ("aux", Some(AuxKind::Decision)) => !has("reranking") && Shape::of(s) == Shape::LabelLogits,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(arch: &str) -> ModelSummary {
        ModelSummary {
            architecture: Some(arch.to_string()),
            ..ModelSummary::default()
        }
    }

    /// A chat row on an aux file is told the class, the kind and the header
    /// key — for a decision model too, including a label-logit type (lev)
    /// whose causal context could still generate.
    #[test]
    fn a_chat_row_on_an_aux_file_is_told_its_class_and_kind() {
        let mut lev = summary("qwen35");
        lev.has_chat_template = true;
        lev.decision_type = Some("lev".into());
        let m = aux_file_on_chat_row("gguf_path 'lev.gguf'", &lev).expect("a finding");
        assert!(
            m.starts_with("gguf_path 'lev.gguf' declares decision.type 'lev'")
                && m.contains("the aux class serves it, as kind=decision")
                && m.contains("/v1/systemone"),
            "{m}"
        );
        assert!(m.contains("target=aux"), "{m}");

        let mut embed = summary("qwen3");
        embed.pooling_type = Some(3);
        let m = aux_file_on_chat_row("this GGUF", &embed).unwrap();
        assert!(
            m.contains("pooling_type 'last'") && m.contains("kind=embed"),
            "{m}"
        );
        let m = aux_file_on_chat_row("this GGUF", &summary("nomic-bert")).unwrap();
        assert!(
            m.contains("is a nomic-bert encoder") && m.contains("kind=embed"),
            "{m}"
        );
        let mut rerank = summary("bert");
        rerank.has_classifier_head = true;
        let m = aux_file_on_chat_row("this GGUF", &rerank).unwrap();
        assert!(
            m.contains("classifier head") && m.contains("kind=rerank"),
            "{m}"
        );

        let mut chat = summary("qwen35");
        chat.has_chat_template = true;
        assert_eq!(aux_file_on_chat_row("this GGUF", &chat), None);
    }

    /// The finding is an advisory while the chat row still loads and serves,
    /// and a problem when it cannot.
    #[test]
    fn a_chat_row_still_serves_only_the_workaround_and_a_label_logit_type() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let mut embed = summary("bert");
        embed.pooling_type = Some(2);
        assert!(!chat_row_still_serves(&embed, &[]), "no LM head");
        for flag in [
            "--embedding",
            "--embeddings",
            "--embedding=true",
            "--reranking",
        ] {
            assert!(chat_row_still_serves(&embed, &args(&[flag])), "{flag}");
        }
        let mut rerank = summary("bert");
        rerank.has_classifier_head = true;
        assert!(!chat_row_still_serves(&rerank, &args(&["--jinja"])));
        assert!(chat_row_still_serves(&rerank, &args(&["--rerank"])));

        // lev: causal, no decision head — label logits, still generates.
        let mut lev = summary("qwen35");
        lev.decision_type = Some("lev".into());
        assert!(chat_row_still_serves(&lev, &[]));
        assert!(
            !chat_row_still_serves(&lev, &args(&["--reranking"])),
            "--reranking crashes a decision model at load"
        );
        // Kev: a causal decision head runs the server in embedding mode.
        let mut kev = lev.clone();
        kev.decision_type = Some("kev".into());
        kev.has_classifier_head = true;
        assert!(!chat_row_still_serves(&kev, &[]));
        // Laya: non-causal.
        let mut laya = summary("modern-bert");
        laya.decision_type = Some("laya".into());
        laya.attention_causal = Some(false);
        assert!(!chat_row_still_serves(&laya, &args(&["--embeddings"])));

        let mut chat = summary("qwen35");
        chat.has_chat_template = true;
        assert!(chat_row_still_serves(&chat, &[]));
    }
}
