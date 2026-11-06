//! A rename never breaks a chain (openai egress design §14.2, decision 27):
//! a renamed plain alias, candidate alias, chat, aux, audio and image row,
//! and a changed chat, aux, decision, audio and image prefix, each rewrite
//! every other kind's fallback that named them, in the rename's own write;
//! a stored name that resolved to something else is left alone; and the
//! answer lists what it rewrote.

use lmgw_core::ops;
use serde_json::{json, Value};

use crate::support::fallback_world::{world, World};
use crate::support::fallbacks::set_fallback;

fn patch<T: serde::de::DeserializeOwned>(v: Value) -> T {
    serde_json::from_value(v).unwrap()
}

/// `fallback` written straight to the row, as a hand edit or an earlier
/// save left it.
async fn names(w: &World, table: &str, owner: &str, fallback: &str) {
    set_fallback(&w.state, table, owner, Some(fallback)).await;
}

#[tokio::test]
async fn a_renamed_alias_and_candidate_alias_are_followed() {
    let w = world().await;
    names(&w, "local_models", "qwen3", "cloud-chat").await;
    names(&w, "candidate_aliases", "smart", "cloud-chat").await;
    names(&w, "aux_models", "bge", "cloud-embed").await;
    let id = w.id("models", "cloud-chat").await;
    let saved = ops::model_set(
        &w.state,
        patch(json!({"action": "update", "id": id, "alias": "cloud-vision"})),
    )
    .await
    .unwrap();
    assert_eq!(
        w.fallback("local_models", "qwen3").await.as_deref(),
        Some("cloud-vision")
    );
    assert_eq!(
        w.fallback("candidate_aliases", "smart").await.as_deref(),
        Some("cloud-vision")
    );
    assert_eq!(
        w.fallback("aux_models", "bge").await.as_deref(),
        Some("cloud-embed")
    );
    let said = saved.fallback_notes.join("\n");
    assert!(
        said.contains("chat model 'qwen3' now names 'cloud-vision' (was 'cloud-chat')"),
        "{said}"
    );

    // A candidate alias a hand edit named is followed too.
    names(&w, "local_models", "other", "smart").await;
    let cid = w.id("candidate_aliases", "smart").await;
    ops::candidate_alias_set(
        &w.state,
        patch(json!({"action": "update", "id": cid, "alias": "clever"})),
    )
    .await
    .unwrap();
    assert_eq!(
        w.fallback("local_models", "other").await.as_deref(),
        Some("clever")
    );
}

#[tokio::test]
async fn a_renamed_row_of_every_class_is_followed() {
    let w = world().await;
    names(&w, "models", "cloud-mini", "qwen3").await;
    names(&w, "aux_models", "bge-rr", "embed/bge").await;
    names(&w, "audio_models", "kokoro", "audio/whisper").await;
    names(&w, "local_models", "other", "image/flux").await;

    let id = w.id("local_models", "qwen3").await;
    ops::local_model_set(
        &w.state,
        patch(json!({"action": "update", "id": id, "model_id": "qwen3-next"})),
    )
    .await
    .unwrap();
    assert_eq!(
        w.fallback("models", "cloud-mini").await.as_deref(),
        Some("qwen3-next")
    );

    let id = w.id("aux_models", "bge").await;
    ops::aux_model_set(
        &w.state,
        patch(json!({"action": "update", "id": id, "model_id": "bge-m3"})),
    )
    .await
    .unwrap();
    assert_eq!(
        w.fallback("aux_models", "bge-rr").await.as_deref(),
        Some("embed/bge-m3")
    );

    let id = w.id("audio_models", "whisper").await;
    ops::audio_model_set(
        &w.state,
        patch(json!({"action": "update", "id": id, "model_id": "parakeet"})),
    )
    .await
    .unwrap();
    assert_eq!(
        w.fallback("audio_models", "kokoro").await.as_deref(),
        Some("audio/parakeet")
    );

    let id = w.id("image_models", "flux").await;
    ops::image_model_set(
        &w.state,
        patch(json!({"action": "update", "id": id, "model_id": "flux-2"})),
    )
    .await
    .unwrap();
    assert_eq!(
        w.fallback("local_models", "other").await.as_deref(),
        Some("image/flux-2")
    );
}

/// A kind change across the aux prefixes renames the row too. (`other`'s
/// fallback names a model of another type already, as migration 0095 may
/// have kept one, so the kind change does not refuse it; a fallback of the
/// reranker's own type would be refused, `fallback_referrers`.)
#[tokio::test]
async fn an_aux_row_turned_into_a_decision_row_is_followed() {
    let w = world().await;
    names(&w, "local_models", "other", "embed/bge-rr").await;
    let id = w.id("aux_models", "bge-rr").await;
    ops::aux_model_set(
        &w.state,
        patch(json!({"action": "update", "id": id, "kind": "decision"})),
    )
    .await
    .unwrap();
    assert_eq!(
        w.fallback("local_models", "other").await.as_deref(),
        Some("decide/bge-rr")
    );
}

/// A stored name an alias shadows names the alias, not the renamed row: it
/// is left as it is, and so is one that resolves to nothing.
#[tokio::test]
async fn a_name_that_resolved_elsewhere_is_left_alone() {
    let w = world().await;
    crate::support::fallback_world::alias(&w.state, "other", w.cloud, None).await;
    w.state.reload_snapshot().await.unwrap();
    names(&w, "models", "cloud-mini", "other").await;
    names(&w, "models", "cloud-b", "gone").await;
    let id = w.id("local_models", "other").await;
    ops::local_model_set(
        &w.state,
        patch(json!({"action": "update", "id": id, "model_id": "other-2"})),
    )
    .await
    .unwrap();
    assert_eq!(
        w.fallback("models", "cloud-mini").await.as_deref(),
        Some("other")
    );
    assert_eq!(
        w.fallback("models", "cloud-b").await.as_deref(),
        Some("gone")
    );
}

/// A changed class prefix renames every local name of its class: the
/// dashboard's settings save rewrites the fallbacks naming them in its own
/// write, class by class.
#[tokio::test]
async fn a_changed_prefix_of_every_class_is_followed() {
    let w = world().await;
    names(&w, "models", "cloud-mini", "qwen3").await;
    names(&w, "models", "cloud-embed", "embed/bge").await;
    names(&w, "models", "cloud-asr", "audio/whisper").await;
    names(&w, "local_models", "other", "image/flux").await;
    sqlx::query("UPDATE aux_models SET kind = 'decision' WHERE model_id = 'bge-rr'")
        .execute(&w.state.db)
        .await
        .unwrap();
    w.state.reload_snapshot().await.unwrap();
    names(&w, "candidate_aliases", "smart", "decide/bge-rr").await;

    let gw = crate::common::serve(w.state.clone()).await;
    let r = gw
        .client()
        .post(format!("{gw}/api/op/settings_set_full"))
        .json(&json!({
            "router": {"public_prefix": "local"},
            "aux_router": {"public_prefix": "emb", "decision_prefix": "sys1"},
            "audio": {"public_prefix": "speech"},
            "image": {"public_prefix": "img"},
        }))
        .send()
        .await
        .unwrap();
    let status = r.status();
    let body: Value = r.json().await.unwrap();
    assert_eq!(status, 200, "{body}");
    for (table, owner, now) in [
        ("models", "cloud-mini", "local/qwen3"),
        ("models", "cloud-embed", "emb/bge"),
        ("models", "cloud-asr", "speech/whisper"),
        ("local_models", "other", "img/flux"),
        ("candidate_aliases", "smart", "sys1/bge-rr"),
    ] {
        assert_eq!(
            w.fallback(table, owner).await.as_deref(),
            Some(now),
            "{owner}"
        );
    }
    // Each rewritten name resolves to the row it named.
    let snap = w.state.snapshot();
    for name in [
        "local/qwen3",
        "emb/bge",
        "speech/whisper",
        "img/flux",
        "sys1/bge-rr",
    ] {
        assert!(snap.hop_key(name).is_some(), "{name}");
    }
}
