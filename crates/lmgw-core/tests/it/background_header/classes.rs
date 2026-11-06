//! The guest branch on the other model classes (per-request background
//! design §7): the gate is class-generic, so an embedder and an image
//! pipeline asked for with `x-lmgw-background: on` start only into room that
//! disturbs nobody, and defer otherwise.

use lmgw_core::config::AuxKind;
use lmgw_core::runtime::registry::Origin;
use serde_json::{json, Value};

use super::*;
use crate::support::gpu_world::GIB;

async fn embed(gw: &Gw, model: &str) -> reqwest::Response {
    gw.client()
        .post(format!("{gw}/v1/embeddings"))
        .header(BACKGROUND, "on")
        .json(&json!({"model": model, "input": "hi"}))
        .send()
        .await
        .unwrap()
}

async fn draw(gw: &Gw, model: &str) -> reqwest::Response {
    gw.client()
        .post(format!("{gw}/v1/images/generations"))
        .header(BACKGROUND, "on")
        .json(&json!({"model": model, "prompt": "a red square"}))
        .send()
        .await
        .unwrap()
}

/// `/v1/embeddings`: with no room beside the owner's chat model the
/// embedder defers and nothing is evicted; with room it is started as the
/// guest's.
#[tokio::test]
async fn an_embedder_defers_without_room_and_starts_as_the_guests_with_it() {
    let (g, gw) = world(24 * GIB, 3).await;
    g.model("x", 20 * GIB).await;
    g.aux("emb", AuxKind::Embed, 6 * GIB).await;
    owner_loads(&g, "x").await;

    let msg = deferred(&g, embed(&gw, "embed/emb").await, "embed/emb", "model").await;
    assert!(msg.contains("GPU in use by chat/x"), "{msg}");
    assert!(
        g.stops().is_empty(),
        "the owner's chat model was not evicted"
    );
    assert_eq!(g.runs(), ["x"]);

    let (g, gw) = world(24 * GIB, 3).await;
    g.model("x", 8 * GIB).await;
    g.aux("emb", AuxKind::Embed, 6 * GIB).await;
    owner_loads(&g, "x").await;
    let resp = embed(&gw, "embed/emb").await;
    assert_eq!(resp.status(), 200, "{:?}", resp.headers());
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["data"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(view(&g, "emb").unwrap().owner, Origin::Background);
    assert!(g.stops().is_empty());
}

/// `/v1/images/generations`: the same for an image pipeline.
#[tokio::test]
async fn an_image_pipeline_defers_without_room_and_starts_as_the_guests_with_it() {
    let (g, gw) = world(24 * GIB, 3).await;
    g.model("x", 20 * GIB).await;
    g.image("z", 6 * GIB).await;
    owner_loads(&g, "x").await;

    let msg = deferred(&g, draw(&gw, "image/z").await, "image/z", "model").await;
    assert!(msg.contains("GPU in use by chat/x"), "{msg}");
    assert!(g.stops().is_empty());
    assert!(g.world().images.is_empty(), "nothing was drawn");

    let (g, gw) = world(24 * GIB, 3).await;
    g.model("x", 8 * GIB).await;
    g.image("z", 6 * GIB).await;
    owner_loads(&g, "x").await;
    let resp = draw(&gw, "image/z").await;
    assert_eq!(resp.status(), 200, "{:?}", resp.headers());
    let v: Value = resp.json().await.unwrap();
    assert!(v["data"][0]["b64_json"].is_string(), "{v}");
    assert_eq!(g.world().images, ["z"]);
    assert_eq!(view(&g, "z").unwrap().owner, Origin::Background);
    assert!(g.stops().is_empty());
}
