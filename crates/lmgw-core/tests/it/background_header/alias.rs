//! `x-lmgw-background` on a candidate alias (per-request background design
//! §5), and an alias's deferral on the wire (§8).

use lmgw_core::runtime::registry::Origin;
use lmgw_core::store::NewCandidateAlias;

use super::*;
use crate::support::gpu_world::GIB;

/// A candidate alias as stored, with no fallback.
fn alias(name: &str, candidates: &[&str], background: bool) -> NewCandidateAlias {
    NewCandidateAlias {
        alias: name.into(),
        candidates: candidates.iter().map(|c| c.to_string()).collect(),
        background,
        fallback: None,
        capabilities_disabled: vec![],
        capabilities_enabled: vec![],
        enabled: true,
        notes: String::new(),
    }
}

/// An alias without the background flag walks as a guest when the request
/// says so: a primary that would need the owner's model evicted is not
/// loaded, and with nothing else loaded and no fallback the alias defers —
/// `503 gpu_deferred` on the wire (decision A), naming the alias. Without the
/// header the same alias loads its primary, evicting as it always has.
#[tokio::test]
async fn the_header_makes_an_owner_alias_walk_as_a_guest() {
    let (g, gw) = world(24 * GIB, 3).await;
    g.model("x", 16 * GIB).await;
    g.model("p", 12 * GIB).await;
    g.candidate(alias("writer", &["p"], false)).await;
    owner_loads(&g, "x").await;

    let msg = deferred(&g, guest_chat(&gw, "writer").await, "writer", "alias").await;
    assert!(msg.contains("GPU in use by chat/x"), "{msg}");
    assert!(g.stops().is_empty(), "the owner's model was not evicted");

    let resp = chat(&gw, "writer", None).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(header(&resp, "x-lmgw-candidate"), Some("p"));
    assert_eq!(g.stops(), ["x"]);
    assert_eq!(view(&g, "p").unwrap().owner, Origin::Owner);
}

/// A primary that fits the free VRAM is started by the guest walk, and is
/// the guest's.
#[tokio::test]
async fn a_guest_walk_on_an_owner_alias_starts_its_primary_as_the_guests() {
    let (g, gw) = world(24 * GIB, 2).await;
    g.model("x", 8 * GIB).await;
    g.model("p", 8 * GIB).await;
    g.candidate(alias("writer", &["p"], false)).await;
    owner_loads(&g, "x").await;

    let resp = guest_chat(&gw, "writer").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(header(&resp, "x-lmgw-candidate"), Some("p"));
    assert_eq!(view(&g, "p").unwrap().owner, Origin::Background);
    assert!(g.stops().is_empty());
}

/// `off` never turns a background alias into foreground traffic: it is a
/// guest whatever the header says, and defers rather than evict.
#[tokio::test]
async fn off_on_a_background_alias_is_still_a_guest() {
    let (g, gw) = world(24 * GIB, 3).await;
    g.model("x", 16 * GIB).await;
    g.model("p", 12 * GIB).await;
    g.candidate(alias("jobs", &["p"], true)).await;
    owner_loads(&g, "x").await;

    deferred(&g, chat(&gw, "jobs", Some("off")).await, "jobs", "alias").await;
    assert!(g.stops().is_empty());
    assert_eq!(g.runs(), ["x"]);
}
