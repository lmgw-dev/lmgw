//! A file from the hub's resolve endpoint, and the commit it came from.
//!
//! The hub answers `resolve/<revision>/<file>` with the commit that revision
//! points at in `X-Repo-Commit` — on its own response, which for any file
//! stored in LFS (every GGUF) is a redirect to a CDN that does not repeat the
//! header (checked 2026-10-02: a 302 to the xet bridge carries it, the CDN's
//! 200 does not; a small file's 307 to `/api/resolve-cache/…` carries it on
//! both). reqwest follows redirects on its own and only hands back the last
//! response, so the hops on the hub are taken without following, each
//! header read, and the first redirect off the hub followed from there. The
//! bytes behind that redirect are content-addressed, so they are the commit
//! the header names, not a later one.

use super::listing::ListFailure;

/// `{base}/{repo}/resolve/{revision}/{file}`.
pub fn resolve_url(repo: &str, revision: &str, file: &str) -> String {
    format!("{}/{repo}/resolve/{revision}/{file}", super::hf_base())
}

/// The commit an `X-Repo-Commit` header names, when it names one.
fn commit_header(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers
        .get("x-repo-commit")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|c| crate::audio::pins::is_commit(c))
        .map(str::to_ascii_lowercase)
}

/// How many redirects on the hub's own origin a resolve may take before its
/// first answer off it — reqwest's own default cap for a whole chain.
const MAX_HUB_HOPS: usize = 10;

/// What the hub's own hops said about the file, beside the response
/// (find-a-model design §2.1). Each is read from a success or a redirect on
/// the hub, the last hop that names one winning, as the commit always was;
/// the CDN repeats none of them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HubHop {
    /// `X-Repo-Commit`: the commit the revision resolved to.
    pub commit: Option<String>,
    /// `X-Linked-Etag`, unquoted: an LFS file's sha256, the tree listing's
    /// `lfs.oid` (D4). The CDN's own `ETag` is another hash (the xet one).
    pub linked_etag: Option<String>,
    /// `X-Linked-Size`: the LFS file's bytes.
    pub linked_size: Option<u64>,
}

impl HubHop {
    fn read(&mut self, headers: &reqwest::header::HeaderMap) {
        let text = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|v| !v.is_empty())
        };
        self.commit = commit_header(headers).or(self.commit.take());
        if let Some(etag) = text("x-linked-etag").map(super::normalize_etag) {
            self.linked_etag = Some(etag)
                .filter(|e| !e.is_empty())
                .or(self.linked_etag.take());
        }
        if let Some(size) = text("x-linked-size").and_then(|v| v.parse().ok()) {
            self.linked_size = Some(size);
        }
    }
}

/// GET `url` (a resolve URL on the hub), and what the hub's hops said about
/// the file ([`HubHop`]). `direct` must not follow redirects; `follow` does
/// the rest of the chain once it leaves the hub. The token goes to the hub
/// only: along redirects that stay on the hub's origin, never to the CDN —
/// the rule reqwest applies itself.
///
/// `cdn_headers` ride the first request off the hub only (find-a-model
/// design §2.1, D40): a `Range` for a header read or a resumed download.
/// The hub's own hops get none, so a hub that answers the file itself (a
/// small file, a mirror) answers it whole, and a caller that sent a `Range`
/// reads a `200` as the whole file.
///
/// Every hop on the hub is taken with `direct` and its header read, the way
/// `huggingface_hub` follows relative redirects itself: a renamed repo
/// answers with a relative redirect to its new name, whose own redirect is
/// the one that names the commit. A hop that names a commit replaces what an
/// earlier one named — the last answer on the hub is the one that resolved
/// the file. The first `Location` off the hub's origin goes to `follow`.
///
/// Every hop asks for the file as it is, `Accept-Encoding: identity` (the
/// `cdn_headers` may say otherwise): reqwest's gzip decoding (find-a-model
/// design D141) asks for gzip on every request it isn't told about, a
/// `Range` included, and a decoded answer has no `Content-Length`, which a
/// download and a header read measure the file by.
///
/// The response may be a failure status; reading it is the caller's
/// business, as it was before. The hop's facts are read only from a success
/// or a redirect: the hub echoes a requested revision on a 404 too.
pub async fn get_with_commit(
    direct: &reqwest::Client,
    follow: &reqwest::Client,
    token: &str,
    url: &str,
    cdn_headers: &reqwest::header::HeaderMap,
) -> Result<(reqwest::Response, HubHop), String> {
    let origin = reqwest::Url::parse(url).map_err(|e| format!("GET {url}: {e}"))?;
    let identity = reqwest::header::HeaderValue::from_static("identity");
    let mut cdn_headers = cdn_headers.clone();
    cdn_headers
        .entry(reqwest::header::ACCEPT_ENCODING)
        .or_insert(identity.clone());
    let mut at = origin.clone();
    let mut seen: Vec<reqwest::Url> = Vec::new();
    let mut hop = HubHop::default();
    while seen.len() < MAX_HUB_HOPS {
        if seen.contains(&at) {
            return Err(format!("GET {url}: the hub's redirects loop back to {at}"));
        }
        seen.push(at.clone());
        let mut rb = direct
            .get(at.clone())
            .header(reqwest::header::ACCEPT_ENCODING, identity.clone());
        if !token.is_empty() {
            rb = rb.bearer_auth(token);
        }
        let resp = rb.send().await.map_err(|e| format!("GET {at}: {e}"))?;
        let status = resp.status();
        if status.is_success() || status.is_redirection() {
            hop.read(resp.headers());
        }
        if !status.is_redirection() {
            return Ok((resp, hop));
        }
        let Some(location) = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
        else {
            // A redirect with nowhere to go: the caller reports the status.
            return Ok((resp, hop));
        };
        let next = at
            .join(location)
            .map_err(|e| format!("GET {at}: redirect to {location}: {e}"))?;
        if !same_origin(&next, &origin) {
            // Off the hub (the CDN): no token, the caller's headers, and
            // reqwest follows the rest.
            let resp = follow
                .get(next.clone())
                .headers(cdn_headers)
                .send()
                .await
                .map_err(|e| format!("GET {next}: {e}"))?;
            return Ok((resp, hop));
        }
        at = next;
    }
    Err(format!(
        "GET {url}: more than {MAX_HUB_HOPS} redirects without leaving the hub"
    ))
}

/// Same scheme, host and port: a protocol-relative `//host`, another host,
/// an http/https switch and another port all leave the hub.
fn same_origin(a: &reqwest::Url, b: &reqwest::Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// A tree listing's failure at `revision` as the sentence a queue answers:
/// a missing pinned revision names the pin. `pin_note` is what a catalog
/// download adds there (no fallback, and the setting that takes the latest).
pub fn listing_refusal(f: ListFailure, pin_note: Option<&str>) -> String {
    match (f, pin_note) {
        (ListFailure::NoRevision(m), Some(note)) => format!("{m} — {note}"),
        (f, _) => f.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    const COMMIT: &str = "607a30d783dfa663caf39e06633721c8d4cfcd7e";

    fn clients() -> (reqwest::Client, reqwest::Client) {
        let direct = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_gzip()
            .build()
            .unwrap();
        (direct, reqwest::Client::new())
    }

    /// [`get_with_commit`] with no headers for the CDN, and the commit only.
    async fn get(
        direct: &reqwest::Client,
        follow: &reqwest::Client,
        token: &str,
        url: &str,
    ) -> Result<(reqwest::Response, Option<String>), String> {
        let none = reqwest::header::HeaderMap::new();
        get_with_commit(direct, follow, token, url, &none)
            .await
            .map(|(resp, hop)| (resp, hop.commit))
    }

    /// The hub's 302 as it answered an anonymous GET of a public projector on
    /// 2026-11-03 (the repo and the bridge path renamed): the hop's facts are
    /// the sha256, the size and the commit, the CDN's `ETag` is the xet hash.
    const RECORDED_SHA256: &str =
        "eacf610d1ee4bd5ed0197a0777dd8f4fceb8eefa27009067c7d496cb68fbde45";
    const RECORDED_XET: &str = "11191dd055c48c57d4074b57ece43a2661ebd6daf5fa5c7b1f9be7c1df03ddd6";
    const RECORDED_SIZE: u64 = 927_607_360;
    const RECORDED_COMMIT: &str = "82d411acf4a06cfb8d9b073a5211bf410bfc29bf";

    /// A `Range` given for the CDN rides the CDN's request only: the hub's
    /// hop gets none and the token, the CDN gets the Range and no token, and
    /// both ask for the file as it is (`identity`, D141). The
    /// hub hop's `x-linked-etag`, `x-linked-size` and `x-repo-commit` come
    /// back beside the CDN's `206` (find-a-model design §2.1, D40).
    #[tokio::test]
    async fn a_range_goes_to_the_cdn_hop_only_and_the_hub_hops_headers_come_back() {
        let hub = MockServer::start().await;
        let cdn = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(
                "/demo-org/demo-model-GGUF/resolve/main/mmproj-F16.gguf",
            ))
            .and(header("authorization", "Bearer tok"))
            .and(header("accept-encoding", "identity"))
            .and(|r: &Request| !r.headers.contains_key("range"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("x-repo-commit", RECORDED_COMMIT)
                    .insert_header("x-linked-etag", format!("\"{RECORDED_SHA256}\"").as_str())
                    .insert_header("x-linked-size", RECORDED_SIZE.to_string().as_str())
                    .insert_header("x-xet-hash", RECORDED_XET)
                    .insert_header("accept-ranges", "bytes")
                    .insert_header(
                        "location",
                        format!("{}/xet-bridge-us/demo/{RECORDED_XET}?Expires=1", cdn.uri())
                            .as_str(),
                    ),
            )
            .expect(1)
            .mount(&hub)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/xet-bridge-us/demo/{RECORDED_XET}")))
            .and(header("range", "bytes=500000000-"))
            // The file as it is, though the client decodes gzip (D141).
            .and(header("accept-encoding", "identity"))
            .and(|r: &Request| !r.headers.contains_key("authorization"))
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("etag", format!("\"{RECORDED_XET}\"").as_str())
                    .insert_header(
                        "content-range",
                        format!("bytes 500000000-500000003/{RECORDED_SIZE}").as_str(),
                    )
                    .set_body_bytes(b"tail".to_vec()),
            )
            .expect(1)
            .mount(&cdn)
            .await;
        let (direct, follow) = clients();
        let url = format!(
            "{}/demo-org/demo-model-GGUF/resolve/main/mmproj-F16.gguf",
            hub.uri()
        );
        let mut range = reqwest::header::HeaderMap::new();
        range.insert(reqwest::header::RANGE, "bytes=500000000-".parse().unwrap());
        let (resp, hop) = get_with_commit(&direct, &follow, "tok", &url, &range)
            .await
            .unwrap();
        assert_eq!(resp.status(), 206);
        assert_eq!(
            hop,
            HubHop {
                commit: Some(RECORDED_COMMIT.into()),
                linked_etag: Some(RECORDED_SHA256.into()),
                linked_size: Some(RECORDED_SIZE),
            }
        );
        assert_eq!(resp.bytes().await.unwrap().as_ref(), b"tail");
    }

    /// The hub's 302 names the commit and points at a CDN that does not:
    /// the commit comes from the first hop, the bytes from the CDN, and the
    /// token never reaches the CDN.
    #[tokio::test]
    async fn the_commit_comes_from_the_hub_hop_and_the_token_stays_there() {
        let hub = MockServer::start().await;
        let cdn = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/o/r/resolve/main/m.gguf"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("x-repo-commit", COMMIT)
                    .insert_header("location", format!("{}/blob/abc", cdn.uri()).as_str()),
            )
            .mount(&hub)
            .await;
        Mock::given(method("GET"))
            .and(path("/blob/abc"))
            .and(|r: &Request| !r.headers.contains_key("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"GGUF".to_vec()))
            .expect(1)
            .mount(&cdn)
            .await;
        let (direct, follow) = clients();
        let url = format!("{}/o/r/resolve/main/m.gguf", hub.uri());
        let (resp, commit) = get(&direct, &follow, "tok", &url).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(commit.as_deref(), Some(COMMIT));
        assert_eq!(resp.bytes().await.unwrap().as_ref(), b"GGUF");
    }

    /// A small file's 307 stays on the hub (a relative Location): followed
    /// with the token. A 200 straight away (a mirror, a mock) carries its own
    /// header or none.
    #[tokio::test]
    async fn a_same_origin_redirect_keeps_the_token_and_a_direct_answer_is_read_as_is() {
        let hub = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/o/r/resolve/main/config.json"))
            .respond_with(
                ResponseTemplate::new(307)
                    .insert_header("x-repo-commit", COMMIT)
                    .insert_header("location", "/api/resolve-cache/o/r/x/config.json"),
            )
            .mount(&hub)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/resolve-cache/o/r/x/config.json"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(1)
            .mount(&hub)
            .await;
        Mock::given(method("GET"))
            .and(path("/o/r/resolve/main/plain.gguf"))
            .respond_with(ResponseTemplate::new(200).set_body_string("x"))
            .mount(&hub)
            .await;
        Mock::given(method("GET"))
            .and(path("/o/r/resolve/main/gone.gguf"))
            .respond_with(ResponseTemplate::new(404).insert_header("x-repo-commit", COMMIT))
            .mount(&hub)
            .await;
        let (direct, follow) = clients();
        let url = |f: &str| format!("{}/o/r/resolve/main/{f}", hub.uri());
        let (resp, commit) = get(&direct, &follow, "tok", &url("config.json"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(commit.as_deref(), Some(COMMIT));
        let (_, commit) = get(&direct, &follow, "", &url("plain.gguf")).await.unwrap();
        assert_eq!(commit, None, "no header, no commit — never a guess");
        let (resp, commit) = get(&direct, &follow, "", &url("gone.gguf")).await.unwrap();
        assert_eq!(resp.status(), 404);
        assert_eq!(commit, None, "a 404 echoes what was asked, not a commit");
    }

    /// A renamed repo: a relative 307 to the new name, whose 302 names the
    /// commit and points at the CDN. Both hub hops carry the token and are
    /// read; the CDN gets neither. A loop on the hub is an error, not a hang.
    #[tokio::test]
    async fn every_hop_on_the_hub_is_read_until_the_first_one_off_it() {
        let hub = MockServer::start().await;
        let cdn = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/old/r/resolve/main/m.gguf"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(
                ResponseTemplate::new(307).insert_header("location", "/new/r/resolve/main/m.gguf"),
            )
            .expect(1)
            .mount(&hub)
            .await;
        Mock::given(method("GET"))
            .and(path("/new/r/resolve/main/m.gguf"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("x-repo-commit", COMMIT)
                    .insert_header("location", format!("{}/blob/m", cdn.uri()).as_str()),
            )
            .expect(1)
            .mount(&hub)
            .await;
        Mock::given(method("GET"))
            .and(path("/blob/m"))
            .and(|r: &Request| !r.headers.contains_key("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"GGUF".to_vec()))
            .expect(1)
            .mount(&cdn)
            .await;
        for (from, to) in [
            ("/a/r/resolve/main/x", "/b/r/resolve/main/x"),
            ("/b/r/resolve/main/x", "/a/r/resolve/main/x"),
        ] {
            Mock::given(method("GET"))
                .and(path(from))
                .respond_with(ResponseTemplate::new(307).insert_header("location", to))
                .mount(&hub)
                .await;
        }
        let (direct, follow) = clients();
        let url = format!("{}/old/r/resolve/main/m.gguf", hub.uri());
        let (resp, commit) = get(&direct, &follow, "tok", &url).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(commit.as_deref(), Some(COMMIT), "from the second hub hop");
        assert_eq!(resp.bytes().await.unwrap().as_ref(), b"GGUF");

        let looped = format!("{}/a/r/resolve/main/x", hub.uri());
        let err = get(&direct, &follow, "", &looped).await.unwrap_err();
        assert!(err.contains("loop back to"), "{err}");
    }

    /// A chain that never repeats a URL (`/hop/1` → `/hop/2` → …) is stopped
    /// by the hop cap alone: [`MAX_HUB_HOPS`] requests, then an error, and
    /// not one request more.
    #[tokio::test]
    async fn an_endless_chain_on_the_hub_stops_at_the_hop_cap() {
        let hub = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(|r: &Request| {
                let n: usize = r
                    .url
                    .path()
                    .trim_start_matches("/hop/")
                    .parse()
                    .unwrap_or(0);
                ResponseTemplate::new(307).insert_header("location", format!("/hop/{}", n + 1))
            })
            .mount(&hub)
            .await;
        let (direct, follow) = clients();
        let url = format!("{}/hop/0", hub.uri());
        // Bounded, so a cap that went missing fails here instead of hanging.
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            get(&direct, &follow, "", &url),
        )
        .await
        .expect("the hop cap ends the chain")
        .unwrap_err();
        assert!(
            err.contains(&format!("more than {MAX_HUB_HOPS} redirects")),
            "{err}"
        );
        assert_eq!(
            hub.received_requests().await.unwrap().len(),
            MAX_HUB_HOPS,
            "the cap, not one request more"
        );
    }
}
