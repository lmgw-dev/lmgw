//! §1.6 with the download's size and §1.5's low-space line against it: the
//! compressed blobs of each missing engine image's registry manifest that
//! the image store does not have yet, each blob once; no size, and why,
//! when a manifest cannot be read.

use lmgw_api_types::machine::{ItemState, MachineCause, MachineItemId};

use crate::support::machine_world::{World, GRAPH_ROOT};

fn item(
    c: &lmgw_api_types::machine::MachineCheck,
    id: MachineItemId,
) -> &lmgw_api_types::machine::MachineItem {
    c.items.iter().find(|i| i.id == id).unwrap()
}

fn digest(c: char) -> String {
    format!("sha256:{}", c.to_string().repeat(64))
}

/// The chat image here; the audio and image engines missing. They share a
/// layer, and the store already has one of the audio engine's: the size is
/// what is left, the shared layer counted once, and with less room than
/// that on the store's file system, the room line says so.
#[tokio::test]
async fn the_download_line_has_the_size_still_to_fetch() {
    let w = World::nvidia().await;
    w.podman
        .images
        .lock()
        .unwrap()
        .insert("ghcr.io/ggml-org/llama.cpp:server-cuda".into());
    let (shared, had, own) = (digest('a'), digest('b'), digest('c'));
    w.manifest(
        "0xshug0/audio.cpp",
        "full-cuda12",
        &digest('1'),
        &[(&shared, 1_000_000_000), (&had, 500_000_000)],
    )
    .await;
    w.manifest(
        "leejet/stable-diffusion.cpp",
        "master-cuda",
        &digest('2'),
        &[(&shared, 1_000_000_000), (&own, 2_000_000_000)],
    )
    .await;
    w.m.facts.file(
        format!("{GRAPH_ROOT}/overlay-layers/layers.json"),
        format!(
            r#"[{{"id":"e0a9","compressed-diff-digest":"{had}","diff-digest":"sha256:e0a9"}},{{"id":"f1","diff-digest":"sha256:f1"}}]"#
        ),
    );
    let c = w.check().await;
    let d = item(&c, MachineItemId::Download);
    assert_eq!(
        d.sentence,
        "Before the first model runs, lmgw downloads what your graphics card needs to run it \
         (2.8 GiB)."
    );
    assert_eq!(d.state, ItemState::Info);
    assert!(matches!(
        &d.cause,
        Some(MachineCause::ImagesMissing { references }) if references.len() == 2
    ));
    assert!(
        d.detail.iter().any(|l| l
            == "ghcr.io/0xshug0/audio.cpp:full-cuda12: not downloaded yet · 954 MiB to download \
                (1 of its 3 blobs are on this computer already)"),
        "{:?}",
        d.detail
    );
    assert!(d
        .detail
        .iter()
        .any(|l| l == "ghcr.io/ggml-org/llama.cpp:server-cuda: on this computer"));
    // Plenty of room on the store's file system: the free figure.
    assert_eq!(
        item(&c, MachineItemId::Room).sentence,
        "Room for models · 466 GiB free."
    );

    // The store's file system with 2 GB free: less than the download.
    w.m.facts
        .set_space("/home", 2_000_000_000, 500_000_000_000, 7);
    let c = w.check().await;
    let room = item(&c, MachineItemId::Room);
    assert_eq!(
        room.sentence,
        "Space is running low: 1.9 GiB free. The download for your graphics card needs 2.8 GiB."
    );
    assert_eq!(room.state, ItemState::Info, "never a problem");
}

/// A manifest the registry does not serve: no size in the sentence, the
/// reason in that image's detail, and no low-space line against a guess.
#[tokio::test]
async fn an_unreadable_manifest_gives_no_size_and_says_why() {
    let w = World::nvidia().await;
    w.m.facts.set_space("/home", 1_000_000, 500_000_000_000, 7);
    let c = w.check().await;
    let d = item(&c, MachineItemId::Download);
    assert_eq!(
        d.sentence,
        "Before the first model runs, lmgw downloads what your graphics card needs to run it."
    );
    assert!(
        d.detail.iter().any(|l| l.starts_with(
            "ghcr.io/leejet/stable-diffusion.cpp:master-cuda: not \
                                    downloaded yet · size not known — "
        ) && l.contains("404")),
        "{:?}",
        d.detail
    );
    assert!(item(&c, MachineItemId::Room)
        .sentence
        .starts_with("Room for models"));
}
