//! The registry update check (container-builds §8) against the real ghcr.io:
//! the anonymous bearer-token flow, the digest a multi-arch tag is served
//! with, and the comparison against the `RepoDigests` podman recorded for
//! the local pull of the two images lmgw's audio and image classes default
//! to.
//!
//! Every other registry test talks to a mock that speaks the protocol as this
//! repo understood it; this is the one that finds out whether ghcr.io still
//! does. It reads the local digests with a read-only `podman image inspect`
//! (an image that is not pulled here is compared with nothing) and makes
//! about three small requests per image — four more when a digest differs and
//! the index is read.
//!
//! Gated on `LMGW_LIVE_REGISTRY=1` because it needs the network:
//!
//! ```sh
//! LMGW_LIVE_REGISTRY=1 cargo test -p lmgw-core --test it backends_registry_live:: -- --ignored --nocapture
//! ```

use lmgw_core::backends::oci::{local_digests, ImageRef, RegistryClient};

const IMAGES: [&str; 2] = [
    "ghcr.io/0xshug0/audio.cpp:full-cuda12",
    "ghcr.io/leejet/stable-diffusion.cpp:master-cuda",
];

/// podman's `RepoDigests` for `reference`, empty when it is not here.
fn repo_digests(reference: &str) -> Vec<String> {
    let out = std::process::Command::new("podman")
        .args([
            "image",
            "inspect",
            "--format",
            "{{json .RepoDigests}}",
            reference,
        ])
        .output();
    match out {
        Ok(o) if o.status.success() => serde_json::from_slice::<Option<Vec<String>>>(&o.stdout)
            .ok()
            .flatten()
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn is_digest(d: &str) -> bool {
    d.strip_prefix("sha256:")
        .is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

#[tokio::test]
#[ignore = "needs the network; set LMGW_LIVE_REGISTRY=1"]
async fn ghcr_serves_what_podman_pulled_or_says_it_moved() {
    if std::env::var("LMGW_LIVE_REGISTRY").unwrap_or_default() != "1" {
        eprintln!("SKIP: set LMGW_LIVE_REGISTRY=1 to ask ghcr.io");
        return;
    }
    let client = RegistryClient::new().unwrap();
    for reference in IMAGES {
        let image = ImageRef::parse(reference).unwrap();
        let local = local_digests(&repo_digests(reference), &image.repo_name());
        let served = client
            .remote_manifest(&image, &local)
            .await
            .unwrap_or_else(|e| panic!("{reference}: {e}"));
        assert!(is_digest(&served.digest), "{reference}: {served:?}");
        assert!(served.members.iter().all(|m| is_digest(m)));
        let verdict = if local.is_empty() {
            "not pulled here — nothing to compare".to_string()
        } else if local.contains(&served.digest) {
            "up to date (the served index digest is one of podman's RepoDigests)".to_string()
        } else if served.matches(&local) {
            "up to date (podman holds the platform manifest of the served index)".to_string()
        } else {
            format!(
                "UPDATE AVAILABLE — the served index has {} member(s), none of them local",
                served.members.len()
            )
        };
        eprintln!(
            "{reference}\n  served: {}\n  local:  {local:?}\n  members read: {}\n  => {verdict}",
            served.digest,
            served.members.len()
        );
    }
}
