//! The one fallback's save checks (openai egress design §14.2, decisions
//! 25–29), through every setter that takes one: `local_model_set`,
//! `aux_model_set`, `audio_model_set`, `image_model_set`,
//! `candidate_alias_set`, `model_set`, and the dashboard's alias route.
//! Each takes `fallback` and `clear: "fallback"` and refuses `""`; a name
//! that does not resolve, the owner itself, a candidate alias, a loop across
//! kinds and a model of another type are refused, naming why; a local model
//! id and a passthrough id are taken; and the save says what a chat
//! fallback lacks or adds, a type it could not check, the same upstream, and
//! what a delete leaves without a working fallback.

use lmgw_core::ops;
use serde_json::{json, Value};

use crate::support::fallback_world::{world, World};

/// One setter's save, as its answer's message and fallback notes, or its
/// refusal.
async fn save(w: &World, setter: &str, patch: Value) -> Result<Vec<String>, String> {
    fn from<T: serde::de::DeserializeOwned>(v: Value) -> T {
        serde_json::from_value(v).unwrap()
    }
    let st = &w.state;
    Ok(match setter {
        "local" => ops::local_model_set(st, from(patch)).await?.fallback_notes,
        "aux" => ops::aux_model_set(st, from(patch)).await?.fallback_notes,
        "audio" => ops::audio_model_set(st, from(patch)).await?.fallback_notes,
        "image" => ops::image_model_set(st, from(patch)).await?.fallback_notes,
        "alias" => ops::model_set(st, from(patch)).await?.fallback_notes,
        _ => match ops::candidate_alias_set(st, from(patch)).await? {
            lmgw_api_types::CandidateAliasAnswer::Saved(s) => s.fallback_notes,
            other => panic!("{other:?}"),
        },
    })
}

/// The patch that updates the owner `name` of `setter`'s kind with `extra`.
async fn update(w: &World, setter: &str, name: &str, extra: Value) -> Value {
    let mut p = match setter {
        "local" | "aux" => json!({"action": "update", "model_id": name}),
        "audio" => json!({"action": "update", "id": w.id("audio_models", name).await}),
        "image" => json!({"action": "update", "id": w.id("image_models", name).await}),
        "alias" => json!({"action": "update", "alias": name}),
        _ => json!({"action": "update", "alias": name}),
    };
    p.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    p
}

/// Every setter, an owner of its kind, a fallback of the owner's type, and
/// the owner's table.
const OWNERS: [(&str, &str, &str, &str); 6] = [
    ("local", "qwen3", "cloud-chat", "local_models"),
    ("aux", "bge", "cloud-embed", "aux_models"),
    ("audio", "whisper", "cloud-asr", "audio_models"),
    ("image", "flux", "kilo/flux-pro", "image_models"),
    ("candidate", "smart", "cloud-mini", "candidate_aliases"),
    ("alias", "cloud-mini", "cloud-chat", "models"),
];

#[tokio::test]
async fn every_setter_takes_a_fallback_keeps_it_and_clear_removes_it() {
    let w = world().await;
    for (setter, owner, fallback, table) in OWNERS {
        let p = update(&w, setter, owner, json!({"fallback": fallback})).await;
        save(&w, setter, p)
            .await
            .unwrap_or_else(|e| panic!("{setter} {owner} → {fallback}: {e}"));
        assert_eq!(
            w.fallback(table, owner).await.as_deref(),
            Some(fallback),
            "{setter}"
        );
        // A save that does not name it keeps it.
        let p = update(&w, setter, owner, json!({})).await;
        save(&w, setter, p).await.unwrap();
        assert_eq!(w.fallback(table, owner).await.as_deref(), Some(fallback));
        let p = update(&w, setter, owner, json!({"clear": "fallback"})).await;
        save(&w, setter, p).await.unwrap();
        assert_eq!(w.fallback(table, owner).await, None, "{setter}");
    }
}

#[tokio::test]
async fn every_setter_refuses_an_empty_name_and_one_that_does_not_resolve() {
    let w = world().await;
    for (setter, owner, _, _) in OWNERS {
        let p = update(&w, setter, owner, json!({"fallback": " "})).await;
        let e = save(&w, setter, p).await.unwrap_err();
        assert!(e.contains("clear: \"fallback\""), "{setter}: {e}");
        let p = update(&w, setter, owner, json!({"fallback": "no-such-model"})).await;
        let e = save(&w, setter, p).await.unwrap_err();
        assert!(
            e.contains("'no-such-model' does not resolve"),
            "{setter}: {e}"
        );
    }
}

#[tokio::test]
async fn itself_a_candidate_alias_and_a_loop_across_kinds_are_refused_by_name() {
    let w = world().await;
    let e = save(
        &w,
        "local",
        update(&w, "local", "qwen3", json!({"fallback": "qwen3"})).await,
    )
    .await
    .unwrap_err();
    assert!(e.contains("would close a loop: qwen3 → qwen3"), "{e}");
    let e = save(
        &w,
        "alias",
        update(&w, "alias", "cloud-chat", json!({"fallback": "SMART"})).await,
    )
    .await
    .unwrap_err();
    assert!(
        e.contains("is a candidate alias") && e.contains("not be a fallback yet"),
        "{e}"
    );

    // A local row → an alias → another local row → back: refused at the
    // save that closes it, naming the loop.
    save(
        &w,
        "local",
        update(&w, "local", "qwen3", json!({"fallback": "cloud-chat"})).await,
    )
    .await
    .unwrap();
    save(
        &w,
        "alias",
        update(&w, "alias", "cloud-chat", json!({"fallback": "other"})).await,
    )
    .await
    .unwrap();
    let e = save(
        &w,
        "local",
        update(&w, "local", "other", json!({"fallback": "qwen3"})).await,
    )
    .await
    .unwrap_err();
    assert!(
        e.contains("fallback 'qwen3' would close a loop: other → qwen3 → cloud-chat → other"),
        "{e}"
    );
}

#[tokio::test]
async fn a_local_model_id_and_a_passthrough_id_are_taken() {
    let w = world().await;
    let notes = save(
        &w,
        "local",
        update(&w, "local", "qwen3", json!({"fallback": "other"})).await,
    )
    .await
    .unwrap();
    assert_eq!(
        w.fallback("local_models", "qwen3").await.as_deref(),
        Some("other")
    );
    assert!(
        notes.iter().all(|n| !n.contains("cannot tell")),
        "{notes:?}"
    );
    // A passthrough id whose catalog cannot be read: saved, with the
    // warning that its type was not checked.
    let notes = save(
        &w,
        "local",
        update(&w, "local", "qwen3", json!({"fallback": "kilo/some-model"})).await,
    )
    .await
    .unwrap();
    assert!(
        notes
            .iter()
            .any(|n| n.contains("cannot tell what type of model 'kilo/some-model' is")),
        "{notes:?}"
    );
}

#[tokio::test]
async fn a_model_of_another_type_is_refused_naming_both() {
    let w = world().await;
    for (setter, owner, fallback, said) in [
        (
            "local",
            "qwen3",
            "embed/bge",
            "'embed/bge' is an embedding model and 'qwen3' is a chat model",
        ),
        (
            "aux",
            "bge",
            "cloud-chat",
            "'cloud-chat' is a chat model and 'embed/bge' is an embedding model",
        ),
        (
            "aux",
            "bge",
            "embed/bge-rr",
            "'embed/bge-rr' is a reranker and 'embed/bge' is an embedding model",
        ),
        (
            "audio",
            "whisper",
            "audio/kokoro",
            "'audio/kokoro' is a text-to-speech model and 'audio/whisper' is a speech-to-text \
             model",
        ),
        (
            "audio",
            "kokoro",
            "cloud-asr",
            "'cloud-asr' is a speech-to-text model and 'audio/kokoro' is a text-to-speech model",
        ),
        (
            "image",
            "flux",
            "qwen3",
            "'qwen3' is a chat model and 'image/flux' is an image model",
        ),
        (
            "candidate",
            "smart",
            "cloud-tts",
            "'cloud-tts' is a text-to-speech model and 'smart' is a chat model",
        ),
        (
            "alias",
            "cloud-embed",
            "cloud-chat",
            "'cloud-chat' is a chat model and 'cloud-embed' is an embedding model",
        ),
    ] {
        let p = update(&w, setter, owner, json!({"fallback": fallback})).await;
        let e = save(&w, setter, p).await.unwrap_err();
        assert!(
            e.contains(said) && e.contains("a fallback must be the same type of model"),
            "{setter} {owner} → {fallback}: {e}"
        );
    }
}

/// Between two chat models the facets may differ: saved, with what the
/// fallback lacks or adds.
#[tokio::test]
async fn a_chat_fallback_with_other_facets_is_saved_with_what_it_lacks_or_adds() {
    let w = world().await;
    let notes = save(
        &w,
        "alias",
        update(&w, "alias", "cloud-chat", json!({"fallback": "cloud-mini"})).await,
    )
    .await
    .unwrap();
    assert!(
        notes
            .iter()
            .any(|n| n.contains("'cloud-mini' is not known to take vision (image input)")),
        "{notes:?}"
    );
    let notes = save(
        &w,
        "alias",
        update(&w, "alias", "cloud-b", json!({"fallback": "cloud-chat"})).await,
    )
    .await
    .unwrap();
    assert!(
        notes
            .iter()
            .any(|n| n.contains("'cloud-chat' takes vision (image input) besides")),
        "{notes:?}"
    );
}

#[tokio::test]
async fn a_type_lmgw_cannot_know_and_the_same_upstream_are_said() {
    let w = world().await;
    let notes = save(
        &w,
        "aux",
        update(&w, "aux", "bge", json!({"fallback": "bare"})).await,
    )
    .await
    .unwrap();
    assert!(
        notes
            .iter()
            .any(|n| n.contains("cannot tell what type of model 'bare' is")),
        "{notes:?}"
    );
    let notes = save(
        &w,
        "alias",
        update(&w, "alias", "cloud-chat", json!({"fallback": "cloud-b"})).await,
    )
    .await
    .unwrap();
    assert!(
        notes
            .iter()
            .any(|n| n.contains("same upstream as 'cloud-chat', so it may share its quota")),
        "{notes:?}"
    );
}

#[tokio::test]
async fn a_delete_names_what_it_leaves_without_a_working_fallback() {
    let w = world().await;
    save(
        &w,
        "local",
        update(&w, "local", "qwen3", json!({"fallback": "other"})).await,
    )
    .await
    .unwrap();
    save(
        &w,
        "alias",
        update(&w, "alias", "cloud-mini", json!({"fallback": "other"})).await,
    )
    .await
    .unwrap();
    let saved = ops::local_model_set(
        &w.state,
        serde_json::from_value(json!({"action": "delete", "model_id": "other"})).unwrap(),
    )
    .await
    .unwrap();
    let said = saved.fallback_notes.join("\n");
    assert!(
        said.contains("chat model 'qwen3' falls back to 'other', which this delete removes"),
        "{said}"
    );
    assert!(
        said.contains("alias 'cloud-mini' falls back to 'other'"),
        "{said}"
    );
}

/// The dashboard's alias route checks the fallback as `model_set` does.
#[tokio::test]
async fn the_dashboard_alias_route_takes_and_clears_a_fallback() {
    let w = world().await;
    let gw = crate::common::serve(w.state.clone()).await;
    let id = w.id("models", "cloud-mini").await;
    let post = |body: Value| {
        let gw = &gw;
        async move {
            let r = gw
                .client()
                .post(format!("{gw}/api/op/alias_set"))
                .json(&body)
                .send()
                .await
                .unwrap();
            (r.status().as_u16(), r.json::<Value>().await.unwrap())
        }
    };
    let (s, v) = post(json!({"action": "update", "id": id, "fallback": "cloud-chat"})).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(
        w.fallback("models", "cloud-mini").await.as_deref(),
        Some("cloud-chat")
    );
    assert!(
        v["fallback_notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n.as_str().unwrap().contains("same upstream")),
        "{v}"
    );
    let (s, v) = post(json!({"action": "update", "id": id, "fallback": "cloud-embed"})).await;
    assert_eq!(s, 400, "{v}");
    let (s, v) = post(json!({"action": "update", "id": id, "clear": "fallback"})).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(w.fallback("models", "cloud-mini").await, None);
}

/// A disabled target is saved, with a note saying what a request reaching
/// it gets until it is enabled again (openai egress design §14.3, decision
/// 28): its own fallback, or a refusal.
#[tokio::test]
async fn a_disabled_target_is_saved_with_a_note() {
    let w = world().await;
    sqlx::query("UPDATE models SET enabled = 0 WHERE alias = 'cloud-mini'")
        .execute(&w.state.db)
        .await
        .unwrap();
    w.state.reload_snapshot().await.unwrap();
    assert!(w.state.snapshot().disabled_key("cloud-mini").is_some());
    let p = update(&w, "local", "qwen3", json!({"fallback": "cloud-mini"})).await;
    let notes = save(&w, "local", p).await.unwrap();
    assert!(
        notes
            .iter()
            .any(|n| n.contains("'cloud-mini' is disabled and has no fallback")),
        "{notes:?}"
    );
    assert_eq!(
        w.fallback("local_models", "qwen3").await.as_deref(),
        Some("cloud-mini")
    );

    crate::support::fallbacks::set_fallback(&w.state, "models", "cloud-mini", Some("cloud-chat"))
        .await;
    crate::support::fallbacks::set_fallback(&w.state, "local_models", "qwen3", None).await;
    let p = update(&w, "local", "qwen3", json!({"fallback": "cloud-mini"})).await;
    let notes = save(&w, "local", p).await.unwrap();
    assert!(
        notes
            .iter()
            .any(|n| n.contains("goes on to its own fallback 'cloud-chat'")),
        "{notes:?}"
    );

    // A disabled local row is typed from the row: an embedder is refused
    // for a chat model as an enabled one is.
    sqlx::query("UPDATE aux_models SET enabled = 0 WHERE model_id = 'bge'")
        .execute(&w.state.db)
        .await
        .unwrap();
    w.state.reload_snapshot().await.unwrap();
    let p = update(&w, "local", "qwen3", json!({"fallback": "embed/bge"})).await;
    let e = save(&w, "local", p).await.unwrap_err();
    assert!(e.contains("must be the same type"), "{e}");

    // A loop through the disabled alias is still one.
    let p = update(&w, "alias", "cloud-chat", json!({"fallback": "qwen3"})).await;
    let e = save(&w, "alias", p).await.unwrap_err();
    assert!(e.contains("loop"), "{e}");
}
