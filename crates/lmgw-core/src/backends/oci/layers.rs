//! The size of a download before it starts (machine-check design §5): the
//! compressed sizes of the blobs — the config and every layer — the
//! registry's manifest lists for this machine's platform, read anonymously
//! like the update check's digest.
//!
//! Podman's compat-mode pull names each blob by the first twelve hex digits
//! of its digest (`report.Id`, measured on 5.8.9: `ee9e6246b78f` for layer
//! `sha256:ee9e6246b78f…`, `e0cffc405270` for the config), so the sizes are
//! keyed the same way ([`short_id`]).

use std::collections::BTreeMap;

use reqwest::header::WWW_AUTHENTICATE;
use reqwest::{Method, StatusCode};
use serde::Deserialize;

use super::{parse_challenge, status_error, ImageRef, RegistryClient};

/// Where a pull's manifest is: a registry repository, and the tag or digest
/// the reference names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRef {
    pub registry: String,
    pub repository: String,
    /// A tag (`server-cuda`) or a digest (`sha256:…`).
    pub reference: String,
}

impl PullRef {
    /// A qualified registry reference — `registry/repository:tag` or
    /// `registry/repository@sha256:…` — as a manifest address; `None` for
    /// what has no registry to ask (a `localhost/` image, an empty one).
    pub fn parse(image: &str) -> Option<Self> {
        match image.split_once('@') {
            Some((name, digest)) => {
                let r = ImageRef::parse(name)?;
                is_digest(digest).then(|| Self {
                    registry: r.registry,
                    repository: r.repository,
                    reference: digest.to_string(),
                })
            }
            None => ImageRef::parse(image).map(|r| Self {
                registry: r.registry,
                repository: r.repository,
                reference: r.tag,
            }),
        }
    }
}

/// `algorithm:hex`, the shape of a content digest.
pub fn is_digest(d: &str) -> bool {
    d.split_once(':').is_some_and(|(algo, hex)| {
        !algo.is_empty()
            && algo
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "+._-".contains(c))
            && hex.len() >= 32
            && hex.chars().all(|c| c.is_ascii_hexdigit())
    })
}

/// The id Podman's compat-mode pull gives a blob: the first twelve hex
/// digits of its digest.
pub fn short_id(digest: &str) -> String {
    let hex = digest.split_once(':').map_or(digest, |(_, h)| h);
    hex.chars().take(12).collect()
}

/// The platform a manifest is picked for, in OCI's words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Platform {
    pub os: &'static str,
    pub architecture: &'static str,
}

impl Platform {
    /// This machine's, as Podman picks a member of a multi-arch index.
    pub fn host() -> Self {
        Self {
            os: "linux",
            architecture: match std::env::consts::ARCH {
                "x86_64" => "amd64",
                "aarch64" => "arm64",
                "x86" => "386",
                "powerpc64" => "ppc64le",
                other => other,
            },
        }
    }
}

/// The blobs a pull of one platform's image downloads, compressed size by
/// [`short_id`]. A blob listed twice is downloaded once, and counted once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Blobs {
    pub sizes: BTreeMap<String, u64>,
}

impl Blobs {
    pub fn total(&self) -> u64 {
        self.sizes.values().sum()
    }
}

#[derive(Deserialize)]
struct Descriptor {
    #[serde(default)]
    digest: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    platform: Option<PlatformDoc>,
}

#[derive(Deserialize)]
struct PlatformDoc {
    #[serde(default)]
    os: String,
    #[serde(default)]
    architecture: String,
}

#[derive(Deserialize)]
struct ManifestDoc {
    /// An index's members (an OCI image index, a Docker manifest list).
    #[serde(default)]
    manifests: Option<Vec<Descriptor>>,
    #[serde(default)]
    config: Option<Descriptor>,
    #[serde(default)]
    layers: Option<Vec<Descriptor>>,
}

/// What one manifest document says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Read {
    /// An index: the digest of its member for the platform asked about.
    Member(String),
    /// An image manifest: its blobs.
    Image(Blobs),
}

/// Read a manifest document `body` for `platform`. Pure; the tests' seam.
pub(crate) fn read(body: &[u8], platform: Platform) -> Result<Read, String> {
    let doc: ManifestDoc = serde_json::from_slice(body)
        .map_err(|e| format!("the registry's manifest is not JSON: {e}"))?;
    if let Some(members) = doc.manifests {
        return members
            .into_iter()
            .find(|m| {
                m.platform
                    .as_ref()
                    .is_some_and(|p| p.os == platform.os && p.architecture == platform.architecture)
            })
            .map(|m| Read::Member(m.digest))
            .ok_or_else(|| {
                format!(
                    "the registry's index has no image for {}/{}",
                    platform.os, platform.architecture
                )
            });
    }
    let (Some(config), Some(layers)) = (doc.config, doc.layers) else {
        return Err("the registry's manifest lists neither members nor layers".into());
    };
    let mut sizes = BTreeMap::new();
    for d in std::iter::once(config).chain(layers) {
        if d.digest.is_empty() {
            return Err("the registry's manifest lists a blob without a digest".into());
        }
        sizes.insert(short_id(&d.digest), d.size);
    }
    Ok(Read::Image(Blobs { sizes }))
}

impl RegistryClient {
    /// The blobs a pull of `image` downloads on `platform`: the tag's (or
    /// digest's) manifest, and for an index its member for the platform.
    /// Anonymous, like [`Self::remote_manifest`].
    pub async fn platform_blobs(
        &self,
        image: &PullRef,
        platform: Platform,
    ) -> Result<Blobs, String> {
        let what = format!("{}/{}", image.registry, image.repository);
        let base = self.api_base(&image.registry);
        let url = |reference: &str| format!("{base}/v2/{}/manifests/{reference}", image.repository);
        let first = url(&image.reference);
        let mut token: Option<String> = None;
        let mut resp = self.send(Method::GET, &first, None, &what).await?;
        if resp.status() == StatusCode::UNAUTHORIZED {
            let challenge = resp
                .headers()
                .get(WWW_AUTHENTICATE)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_challenge)
                .ok_or_else(|| {
                    format!(
                        "{what}: the registry wants credentials (401 with no bearer challenge) — \
                         lmgw reads public manifests only, anonymously"
                    )
                })?;
            let as_ref = ImageRef {
                registry: image.registry.clone(),
                repository: image.repository.clone(),
                tag: image.reference.clone(),
            };
            token = Some(self.token(&challenge, &as_ref, &what).await?);
            resp = self
                .send(Method::GET, &first, token.as_deref(), &what)
                .await?;
        }
        let status = resp.status();
        if !status.is_success() {
            return Err(status_error(&what, status, resp.headers(), token.is_some()));
        }
        let body = resp
            .bytes()
            .await
            .map_err(|e| format!("{what}: reading the registry's manifest: {e}"))?;
        match read(&body, platform).map_err(|e| format!("{what}: {e}"))? {
            Read::Image(blobs) => Ok(blobs),
            Read::Member(digest) => {
                let body = self
                    .manifest_body(&url(&digest), token.as_deref(), &what)
                    .await?;
                match read(&body, platform).map_err(|e| format!("{what}: {e}"))? {
                    Read::Image(blobs) => Ok(blobs),
                    Read::Member(_) => Err(format!(
                        "{what}: the registry's index points at another index"
                    )),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AMD64: Platform = Platform {
        os: "linux",
        architecture: "amd64",
    };

    /// The shape of `docker.io/library/golang:1.24-bookworm`'s amd64
    /// manifest as the registry served it on 2026-11-03 (sizes and digest
    /// prefixes as read, the rest of each digest shortened).
    #[test]
    fn an_image_manifest_gives_config_and_layers_by_short_id() {
        let body = br#"{
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                       "digest": "sha256:e0cffc4052700000000000000000000000000000000000000000000000000000", "size": 3064},
            "layers": [
              {"digest": "sha256:6bc9f599b3ef0000000000000000000000000000000000000000000000000000", "size": 48481483},
              {"digest": "sha256:89edcaae7ec40000000000000000000000000000000000000000000000000000", "size": 24038446},
              {"digest": "sha256:bbceb00354290000000000000000000000000000000000000000000000000000", "size": 64395971},
              {"digest": "sha256:d1ae0a59862a0000000000000000000000000000000000000000000000000000", "size": 92433428},
              {"digest": "sha256:f7bdfd728ac20000000000000000000000000000000000000000000000000000", "size": 79128635},
              {"digest": "sha256:ee9e6246b78f0000000000000000000000000000000000000000000000000000", "size": 125},
              {"digest": "sha256:4f4fb700ef540000000000000000000000000000000000000000000000000000", "size": 32},
              {"digest": "sha256:4f4fb700ef540000000000000000000000000000000000000000000000000000", "size": 32}
            ]
        }"#;
        let Read::Image(blobs) = read(body, AMD64).unwrap() else {
            panic!("an image manifest");
        };
        assert_eq!(blobs.sizes.len(), 8, "the empty layer twice is one blob");
        assert_eq!(blobs.sizes["e0cffc405270"], 3064);
        assert_eq!(blobs.sizes["4f4fb700ef54"], 32);
        assert_eq!(blobs.total(), 308_478_120 + 3064);
    }

    #[test]
    fn an_index_gives_the_member_for_the_platform() {
        let body = br#"{
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [
              {"digest": "sha256:aaaa", "size": 1, "platform": {"architecture": "arm64", "os": "linux", "variant": "v8"}},
              {"digest": "sha256:attestation", "size": 1, "platform": {"architecture": "unknown", "os": "unknown"}},
              {"digest": "sha256:bbbb", "size": 1, "platform": {"architecture": "amd64", "os": "linux"}}
            ]
        }"#;
        assert_eq!(
            read(body, AMD64).unwrap(),
            Read::Member("sha256:bbbb".into())
        );
        let e = read(
            body,
            Platform {
                os: "linux",
                architecture: "s390x",
            },
        )
        .unwrap_err();
        assert!(e.contains("no image for linux/s390x"), "{e}");
        assert!(read(b"{}", AMD64).is_err());
        assert!(read(b"not json", AMD64).is_err());
    }

    #[test]
    fn pull_refs_take_a_tag_or_a_digest() {
        let d = format!("sha256:{}", "ab".repeat(32));
        assert_eq!(
            PullRef::parse("ghcr.io/ggml-org/llama.cpp:server-cuda"),
            Some(PullRef {
                registry: "ghcr.io".into(),
                repository: "ggml-org/llama.cpp".into(),
                reference: "server-cuda".into(),
            })
        );
        assert_eq!(
            PullRef::parse(&format!("busybox@{d}")),
            Some(PullRef {
                registry: "docker.io".into(),
                repository: "library/busybox".into(),
                reference: d.clone(),
            })
        );
        assert_eq!(PullRef::parse("busybox@sha256:short"), None);
        assert_eq!(PullRef::parse("localhost/lmgw-llama-server:x"), None);
        assert_eq!(short_id(&d), "abababababab");
        assert!(is_digest(&d));
        assert!(!is_digest("sha256:"));
        assert!(!is_digest("latest"));
    }
}
