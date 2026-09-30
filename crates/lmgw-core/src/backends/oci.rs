//! The registry half of the update check (container-builds design §8, last
//! bullet): what digest a registry serves for a tag right now, asked through
//! the OCI distribution API (`/v2/<name>/manifests/<tag>`) with the anonymous
//! bearer-token flow every public registry speaks — ghcr.io, Docker Hub,
//! quay.io alike.
//!
//! # The flow
//!
//! `HEAD` the manifest without credentials. A public registry answers `401`
//! with a challenge, `Bearer realm="https://ghcr.io/token",service="ghcr.io",
//! scope="repository:<name>:pull"`; the realm hands out an anonymous pull
//! token for exactly that scope, and the `HEAD` is repeated with it. The
//! digest is the `Docker-Content-Digest` header (or the sha256 of the body,
//! for a registry that omits it on `HEAD`). The token is asked for anonymously
//! and sent only to the registry that challenged for it; lmgw holds no
//! registry credentials, so a private image is reported as such, not checked.
//!
//! # Index or manifest
//!
//! A multi-arch tag is an index (an OCI image index or a Docker manifest
//! list); the registry answers with the **index** digest. podman's
//! `RepoDigests` for such a pull normally carries the index digest too, so the
//! plain comparison is right. Where it carries only the platform manifest's
//! digest, that comparison would call every multi-arch image outdated — so
//! when the served digest is not one podman holds, the manifest is read, and
//! an index whose members include a local digest is up to date. That read
//! happens only on a mismatch: a steady-state check is two or three small
//! requests per image.

use std::collections::BTreeMap;
use std::time::Duration;

use reqwest::header::{ACCEPT, WWW_AUTHENTICATE};
use reqwest::{Method, StatusCode, Url};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Docker Hub's name in image references.
pub const DOCKER_HUB: &str = "docker.io";

/// Where Docker Hub's registry API is — not on `docker.io` itself.
const DOCKER_HUB_API: &str = "https://registry-1.docker.io";

/// Every manifest shape the check understands, index kinds first: a registry
/// answering a multi-arch tag must be allowed to say so, or it converts (and
/// the digest is no longer the tag's).
const ACCEPT_MANIFESTS: &str = "application/vnd.oci.image.index.v1+json, \
     application/vnd.docker.distribution.manifest.list.v2+json, \
     application/vnd.docker.distribution.manifest.v2+json, \
     application/vnd.oci.image.manifest.v1+json";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

const USER_AGENT: &str = concat!("lmgw/", env!("CARGO_PKG_VERSION"));

// ---------------------------------------------------------------------------
// References
// ---------------------------------------------------------------------------

/// A registry image reference, parsed the way podman qualifies one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    /// `ghcr.io`, `docker.io`, `quay.io`, `host:5000` — lowercased.
    pub registry: String,
    /// The repository path: `0xshug0/audio.cpp`, `library/busybox`.
    pub repository: String,
    pub tag: String,
}

impl ImageRef {
    /// `None` for what has no registry to ask: a `localhost/` image (built
    /// here), a reference pinned by digest (it cannot move), an empty one.
    /// A bare name is Docker Hub's, as podman's own spelling of it says
    /// (`docker.io/library/busybox:latest`).
    pub fn parse(reference: &str) -> Option<Self> {
        let r = reference.trim();
        if r.is_empty() || r.contains('@') || r.chars().any(char::is_whitespace) {
            return None;
        }
        let (first, rest) = r.split_once('/').unwrap_or(("", r));
        let explicit = !first.is_empty()
            && (first.contains('.') || first.contains(':') || first == "localhost");
        let (registry, path) = if explicit {
            (first.to_ascii_lowercase(), rest.to_string())
        } else if r.contains('/') {
            (DOCKER_HUB.to_string(), r.to_string())
        } else {
            (DOCKER_HUB.to_string(), format!("library/{r}"))
        };
        if registry == "localhost" || registry.starts_with("localhost:") {
            return None;
        }
        let registry = match registry.as_str() {
            "index.docker.io" | "registry-1.docker.io" => DOCKER_HUB.to_string(),
            _ => registry,
        };
        let (repository, tag) = match path.rsplit_once('/') {
            Some((dir, last)) => match last.split_once(':') {
                Some((name, tag)) => (format!("{dir}/{name}"), tag.to_string()),
                None => (path.clone(), "latest".to_string()),
            },
            None => match path.split_once(':') {
                Some((name, tag)) => (name.to_string(), tag.to_string()),
                None => (path.clone(), "latest".to_string()),
            },
        };
        if repository.is_empty() || tag.is_empty() || repository.split('/').any(str::is_empty) {
            return None;
        }
        Some(Self {
            registry,
            repository,
            tag,
        })
    }

    /// `ghcr.io/0xshug0/audio.cpp` — what podman's `RepoDigests` entries of
    /// this repository start with (before the `@`).
    pub fn repo_name(&self) -> String {
        format!("{}/{}", self.registry, self.repository)
    }

    /// The fully qualified reference, `registry/repository:tag`.
    pub fn reference(&self) -> String {
        format!("{}:{}", self.repo_name(), self.tag)
    }
}

/// The `sha256:…` digests among podman's `RepoDigests` (`repo@sha256:…`)
/// that belong to `repo_name` ([`ImageRef::repo_name`]).
pub fn local_digests(repo_digests: &[String], repo_name: &str) -> Vec<String> {
    let mut out: Vec<String> = repo_digests
        .iter()
        .filter_map(|d| d.split_once('@'))
        .filter(|(repo, _)| repo.eq_ignore_ascii_case(repo_name))
        .map(|(_, digest)| digest.to_string())
        .collect();
    out.dedup();
    out
}

// ---------------------------------------------------------------------------
// The client
// ---------------------------------------------------------------------------

/// What the registry serves for a tag.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteManifest {
    /// `sha256:…` of what the tag points at — an index for a multi-arch tag.
    pub digest: String,
    /// The index's member manifests' digests — read only when `digest` was
    /// not among the local digests the check was given; empty otherwise, and
    /// for a single-platform manifest.
    pub members: Vec<String>,
}

impl RemoteManifest {
    /// Whether an image holding `local` (its `sha256:…` digests of this
    /// repository) is what the registry serves: the tag's digest itself, or
    /// — for an index — one of its members.
    pub fn matches(&self, local: &[String]) -> bool {
        local.contains(&self.digest) || self.members.iter().any(|m| local.contains(m))
    }
}

/// Anonymous registry-API calls. Endpoints are the real registries unless a
/// test points one at a mock ([`Self::with_endpoint`]).
#[derive(Debug, Clone)]
pub struct RegistryClient {
    http: reqwest::Client,
    endpoints: BTreeMap<String, String>,
}

/// A `WWW-Authenticate: Bearer …` challenge.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Challenge {
    realm: String,
    service: Option<String>,
    scope: Option<String>,
}

#[derive(Deserialize)]
struct TokenAnswer {
    token: Option<String>,
    access_token: Option<String>,
}

#[derive(Deserialize)]
struct IndexDoc {
    #[serde(default)]
    manifests: Vec<IndexMember>,
}

#[derive(Deserialize)]
struct IndexMember {
    #[serde(default)]
    digest: String,
}

impl RegistryClient {
    pub fn new() -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .user_agent(USER_AGENT)
            .build()
            .map_err(|e| format!("building the registry HTTP client: {e}"))?;
        Ok(Self {
            http,
            endpoints: BTreeMap::new(),
        })
    }

    /// Serve `registry`'s API from `base` (`http://127.0.0.1:<port>`) — a mock
    /// in tests.
    pub fn with_endpoint(mut self, registry: &str, base: impl Into<String>) -> Self {
        self.endpoints.insert(
            registry.to_ascii_lowercase(),
            base.into().trim_end_matches('/').to_string(),
        );
        self
    }

    fn api_base(&self, registry: &str) -> String {
        if let Some(b) = self.endpoints.get(registry) {
            return b.clone();
        }
        if registry == DOCKER_HUB {
            DOCKER_HUB_API.to_string()
        } else {
            format!("https://{registry}")
        }
    }

    /// What the registry serves for `image`'s tag right now. `local` is the
    /// digests the local image holds for this repository: when the served
    /// digest is not among them, the manifest is read to see whether it is an
    /// index containing one of them (see the module docs).
    pub async fn remote_manifest(
        &self,
        image: &ImageRef,
        local: &[String],
    ) -> Result<RemoteManifest, String> {
        let what = image.reference();
        let url = format!(
            "{}/v2/{}/manifests/{}",
            self.api_base(&image.registry),
            image.repository,
            image.tag
        );
        let mut token: Option<String> = None;
        let mut resp = self.send(Method::HEAD, &url, None, &what).await?;
        if resp.status() == StatusCode::UNAUTHORIZED {
            let challenge = resp
                .headers()
                .get(WWW_AUTHENTICATE)
                .and_then(|v| v.to_str().ok())
                .and_then(parse_challenge)
                .ok_or_else(|| {
                    format!(
                        "{what}: the registry wants credentials (401 with no bearer challenge) — \
                         lmgw checks public images only, anonymously"
                    )
                })?;
            token = Some(self.token(&challenge, image, &what).await?);
            resp = self
                .send(Method::HEAD, &url, token.as_deref(), &what)
                .await?;
        }
        let status = resp.status();
        if !status.is_success() {
            return Err(status_error(&what, status, resp.headers(), token.is_some()));
        }
        let header_digest = resp
            .headers()
            .get("docker-content-digest")
            .and_then(|v| v.to_str().ok())
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty());
        let (digest, body) = match header_digest {
            Some(d) if local.contains(&d) => {
                return Ok(RemoteManifest {
                    digest: d,
                    members: Vec::new(),
                })
            }
            Some(d) => (d, self.manifest_body(&url, token.as_deref(), &what).await?),
            // No digest on the HEAD: the body's own sha256 is the digest.
            None => {
                let body = self.manifest_body(&url, token.as_deref(), &what).await?;
                (
                    format!("sha256:{}", hex::encode(Sha256::digest(&body))),
                    body,
                )
            }
        };
        let members = serde_json::from_slice::<IndexDoc>(&body)
            .map(|doc| {
                doc.manifests
                    .into_iter()
                    .map(|m| m.digest)
                    .filter(|d| !d.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        Ok(RemoteManifest { digest, members })
    }

    async fn manifest_body(
        &self,
        url: &str,
        token: Option<&str>,
        what: &str,
    ) -> Result<Vec<u8>, String> {
        let resp = self.send(Method::GET, url, token, what).await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(status_error(what, status, resp.headers(), token.is_some()));
        }
        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| format!("{what}: reading the registry's manifest: {e}"))
    }

    async fn send(
        &self,
        method: Method,
        url: &str,
        token: Option<&str>,
        what: &str,
    ) -> Result<reqwest::Response, String> {
        let mut rb = self
            .http
            .request(method, url)
            .timeout(REQUEST_TIMEOUT)
            .header(ACCEPT, ACCEPT_MANIFESTS);
        if let Some(t) = token {
            rb = rb.bearer_auth(t);
        }
        rb.send()
            .await
            .map_err(|e| format!("{what}: the registry did not answer: {e}"))
    }

    /// An anonymous pull token for `image` from the challenge's realm.
    async fn token(&self, ch: &Challenge, image: &ImageRef, what: &str) -> Result<String, String> {
        let mut url = Url::parse(&ch.realm).map_err(|e| {
            format!(
                "{what}: the registry's token realm '{}' is not a URL: {e}",
                ch.realm
            )
        })?;
        if !matches!(url.scheme(), "https" | "http") {
            return Err(format!(
                "{what}: the registry's token realm '{}' is not an HTTP(S) URL",
                ch.realm
            ));
        }
        let scope = ch
            .scope
            .clone()
            .unwrap_or_else(|| format!("repository:{}:pull", image.repository));
        {
            let mut q = url.query_pairs_mut();
            if let Some(s) = &ch.service {
                q.append_pair("service", s);
            }
            q.append_pair("scope", &scope);
        }
        let resp = self
            .http
            .get(url)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|e| format!("{what}: the registry's token service did not answer: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!(
                "{what}: the registry's token service answered {} — the image may be private, \
                 which lmgw does not check",
                status.as_u16()
            ));
        }
        let answer: TokenAnswer = resp
            .json()
            .await
            .map_err(|e| format!("{what}: the registry's token answer is not JSON: {e}"))?;
        answer
            .token
            .or(answer.access_token)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| format!("{what}: the registry's token answer carries no token"))
    }
}

/// `Bearer realm="…",service="…",scope="…"` → its parts. `None` for any
/// other scheme (`Basic` wants credentials lmgw does not have).
fn parse_challenge(header: &str) -> Option<Challenge> {
    let rest = header.trim();
    let (scheme, params) = rest.split_once(char::is_whitespace)?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let mut ch = Challenge::default();
    let mut chars = params.chars().peekable();
    loop {
        while matches!(chars.peek(), Some(c) if c.is_whitespace() || *c == ',') {
            chars.next();
        }
        let key: String = chars.by_ref().take_while(|c| *c != '=').collect();
        if key.is_empty() {
            break;
        }
        let value: String = if chars.peek() == Some(&'"') {
            chars.next();
            let mut v = String::new();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => {
                        if let Some(n) = chars.next() {
                            v.push(n);
                        }
                    }
                    '"' => break,
                    c => v.push(c),
                }
            }
            v
        } else {
            chars.by_ref().take_while(|c| *c != ',').collect()
        };
        match key.trim().to_ascii_lowercase().as_str() {
            "realm" => ch.realm = value,
            "service" => ch.service = Some(value),
            "scope" => ch.scope = Some(value),
            _ => {}
        }
    }
    (!ch.realm.is_empty()).then_some(ch)
}

/// A non-2xx manifest answer in words.
fn status_error(
    what: &str,
    status: StatusCode,
    headers: &reqwest::header::HeaderMap,
    token_sent: bool,
) -> String {
    let code = status.as_u16();
    match status {
        StatusCode::NOT_FOUND => format!("{what}: the registry has no such tag (404)"),
        StatusCode::TOO_MANY_REQUESTS => {
            let after = headers
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .map(|s| format!(" — it says to retry after {} s", s.trim()))
                .unwrap_or_default();
            format!("{what}: the registry rate-limited the check (429){after}")
        }
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN if token_sent => format!(
            "{what}: the registry refused an anonymous pull token ({code}) — a private image, \
             which lmgw does not check"
        ),
        _ => format!("{what}: the registry answered {code}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(reference: &str) -> Option<(String, String, String)> {
        ImageRef::parse(reference).map(|i| (i.registry, i.repository, i.tag))
    }

    fn t(a: &str, b: &str, c: &str) -> Option<(String, String, String)> {
        Some((a.into(), b.into(), c.into()))
    }

    #[test]
    fn references_qualify_like_podman() {
        assert_eq!(
            r("ghcr.io/0xshug0/audio.cpp:full-cuda12"),
            t("ghcr.io", "0xshug0/audio.cpp", "full-cuda12")
        );
        assert_eq!(
            r("ghcr.io/leejet/stable-diffusion.cpp:master-cuda"),
            t("ghcr.io", "leejet/stable-diffusion.cpp", "master-cuda")
        );
        assert_eq!(r("busybox"), t("docker.io", "library/busybox", "latest"));
        assert_eq!(
            r("nvidia/cuda:13.0.0-devel-ubuntu24.04"),
            t("docker.io", "nvidia/cuda", "13.0.0-devel-ubuntu24.04")
        );
        assert_eq!(
            r("docker.io/library/postgres:17-alpine"),
            t("docker.io", "library/postgres", "17-alpine")
        );
        assert_eq!(
            r("index.docker.io/library/busybox"),
            t("docker.io", "library/busybox", "latest")
        );
        assert_eq!(r("Quay.IO/a/b/c:1"), t("quay.io", "a/b/c", "1"));
        assert_eq!(r("host:5000/x:y"), t("host:5000", "x", "y"));
        // Nothing to ask.
        assert_eq!(r("localhost/lmgw-llama-server:official-master"), None);
        assert_eq!(r("localhost:5000/x:y"), None);
        assert_eq!(r(&format!("ghcr.io/o/r@sha256:{}", "a".repeat(64))), None);
        assert_eq!(r(""), None);
        assert_eq!(r("ghcr.io//x"), None);
        let i = ImageRef::parse("ghcr.io/o/r").unwrap();
        assert_eq!(i.reference(), "ghcr.io/o/r:latest");
        assert_eq!(i.repo_name(), "ghcr.io/o/r");
    }

    #[test]
    fn local_digests_are_this_repositorys_only() {
        let d = |c: char| format!("sha256:{}", c.to_string().repeat(64));
        let repo_digests = vec![
            format!("ghcr.io/o/r@{}", d('a')),
            format!("ghcr.io/o/r@{}", d('b')),
            format!("ghcr.io/o/other@{}", d('c')),
        ];
        assert_eq!(
            local_digests(&repo_digests, "ghcr.io/o/r"),
            vec![d('a'), d('b')]
        );
        assert!(local_digests(&repo_digests, "docker.io/o/r").is_empty());
    }

    #[test]
    fn an_index_matches_through_its_members() {
        let m = RemoteManifest {
            digest: "sha256:index".into(),
            members: vec!["sha256:amd64".into(), "sha256:arm64".into()],
        };
        assert!(m.matches(&["sha256:index".into()]));
        assert!(m.matches(&["sha256:amd64".into()]));
        assert!(!m.matches(&["sha256:old".into()]));
        assert!(!m.matches(&[]));
    }

    #[test]
    fn challenges_parse_with_quotes_and_commas() {
        let ch = parse_challenge(
            r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:o/r:pull""#,
        )
        .unwrap();
        assert_eq!(ch.realm, "https://ghcr.io/token");
        assert_eq!(ch.service.as_deref(), Some("ghcr.io"));
        assert_eq!(ch.scope.as_deref(), Some("repository:o/r:pull"));
        let ch = parse_challenge(
            r#"bearer realm="https://auth.docker.io/token", scope="repository:a/b:pull,push""#,
        )
        .unwrap();
        assert_eq!(ch.scope.as_deref(), Some("repository:a/b:pull,push"));
        assert_eq!(ch.service, None);
        assert!(parse_challenge(r#"Basic realm="registry""#).is_none());
        assert!(parse_challenge("Bearer service=\"x\"").is_none());
    }
}
