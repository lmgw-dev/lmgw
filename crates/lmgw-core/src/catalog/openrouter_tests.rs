//! OpenRouter's full list in the catalog's own rules (decision-models design
//! §6.2): the task each output names, which models a sync leaves unpriced,
//! and the scopes an aliased model gets on a passthrough upstream.

use serde_json::json;

use super::*;

/// The task an entry stating `outputs` gets.
fn task_of(outputs: &[&str]) -> Option<String> {
    parse_task_openai(&json!({"architecture": {"output_modalities": outputs}}))
}

#[test]
fn each_output_the_full_list_names_takes_its_task() {
    for (outputs, task) in [
        (&["decisions"][..], "decision"),
        (&["embeddings"], "embedding"),
        (&["embedding"], "embedding"),
        (&["rerank"], "rerank"),
        (&["speech"], "tts"),
        (&["transcription"], "asr"),
        (&["image"], "image_generation"),
        (&["video"], "video_generation"),
        (&["image", "video"], "image_generation"),
        (&["text"], "chat"),
        (&["image", "text"], "chat"),
        (&["text", "audio"], "chat"),
        (&["audio"], "chat"),
    ] {
        assert_eq!(task_of(outputs).as_deref(), Some(task), "{outputs:?}");
    }
    assert_eq!(parse_task_openai(&json!({"id": "gpt-4o"})), None);
}

fn priced(id: &str, task: &str, prompt: &str, completion: &str, other: &[&str]) -> ModelInfo {
    ModelInfo {
        id: id.to_string(),
        task: Some(task.to_string()),
        pricing: Some(Pricing {
            prompt: prompt.to_string(),
            completion: completion.to_string(),
            other: other.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// A model billed in a unit the token fields do not hold, or free tokens
/// beside a charging field no row takes, writes no row and counts as
/// unpriced; a really free model keeps its 0 row.
#[test]
fn what_cannot_be_priced_per_token_is_left_unpriced() {
    let models = [
        priced("fish-audio/drama-3", "tts", "0.000002", "0", &[]),
        priced(
            "elevenlabs/scribe-v2",
            "asr",
            "0.00003055555555555",
            "0",
            &[],
        ),
        priced("voyageai/rerank-3-lite", "rerank", "0", "0", &[]),
        priced("x-ai/grok-imagine-video", "video_generation", "0", "0", &[]),
        priced(
            "tencent/hy-image",
            "image_generation",
            "0",
            "0",
            &["image_output"],
        ),
        priced("vendor/talker", "chat", "0", "0", &["audio_output"]),
        priced("liquid/embed:free", "embedding", "0", "0", &[]),
        priced("typesafe/jev-1.13", "decision", "0.000000042", "0", &[]),
    ];
    let updates = price_updates_for_upstream(8, &models, &[], false);
    assert_eq!(updates.unpriced_models, 6);
    let keys: Vec<&str> = updates.rows.iter().map(|r| r.scope_key.as_str()).collect();
    assert_eq!(keys, ["8:liquid/embed:free", "8:typesafe/jev-1.13"]);
    assert!(
        updates.not_synced.is_empty(),
        "an unpriced model's fields are not counted as synced around: {:?}",
        updates.not_synced
    );
}

/// On a passthrough upstream an aliased model is priced under its alias and
/// under its upstream key, so `jev` and `openrouter/typesafe/jev-1.13` both
/// find a row; without passthrough the alias alone, as before.
#[test]
fn a_passthrough_upstream_prices_an_aliased_model_under_both_names() {
    let models = [priced(
        "typesafe/jev-1.13",
        "decision",
        "0.000000042",
        "0",
        &[],
    )];
    let aliases = [("typesafe/jev-1.13", "jev")];
    let scopes = |passthrough| {
        price_updates_for_upstream(8, &models, &aliases, passthrough)
            .rows
            .into_iter()
            .map(|r| (r.scope_kind, r.scope_key))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        scopes(true),
        [
            (PriceScope::Alias, "jev".to_string()),
            (PriceScope::UpstreamModel, "8:typesafe/jev-1.13".to_string()),
        ]
    );
    assert_eq!(scopes(false), [(PriceScope::Alias, "jev".to_string())]);
}
