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

/// GET `url` (a resolve URL on the hub) and the commit the hub resolved it
/// to. `direct` must not follow redirects; `follow` does the rest of the
/// chain once it leaves the hub. The token goes to the hub only: along
/// redirects that stay on the hub's origin, never to the CDN — the rule
/// reqwest applies itself.
///
/// Every hop on the hub is taken with `direct` and its header read, the way
/// `huggingface_hub` follows relative redirects itself: a renamed repo
/// answers with a relative redirect to its new name, whose own redirect is
/// the one that names the commit. A hop that names a commit replaces what an
/// earlier one named — the last answer on the hub is the one that resolved
/// the file. The first `Location` off the hub's origin goes to `follow`.
///
/// The response may be a failure status; reading it is the caller's
/// business, as it was before. The commit is read only from a success or a
/// redirect: the hub echoes a requested revision on a 404 too.
pub async fn get_with_commit(
    direct: &reqwest::Client,
    follow: &reqwest::Client,
    token: &str,
    url: &str,
) -> Result<(reqwest::Response, Option<String>), String> {
    let origin = reqwest::Url::parse(url).map_err(|e| format!("GET {url}: {e}"))?;
    let mut at = origin.clone();
    let mut seen: Vec<reqwest::Url> = Vec::new();
    let mut commit = None;
    while seen.len() < MAX_HUB_HOPS {
        if seen.contains(&at) {
            return Err(format!("GET {url}: the hub's redirects loop back to {at}"));
        }
        seen.push(at.clone());
        let mut rb = direct.get(at.clone());
        if !token.is_empty() {
            rb = rb.bearer_auth(token);
        }
        let resp = rb.send().await.map_err(|e| format!("GET {at}: {e}"))?;
        let status = resp.status();
        if status.is_success() || status.is_redirection() {
            commit = commit_header(resp.headers()).or(commit);
        }
        if !status.is_redirection() {
            return Ok((resp, commit));
        }
        let Some(location) = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
        else {
            // A redirect with nowhere to go: the caller reports the status.
            return Ok((resp, commit));
        };
        let next = at
            .join(location)
            .map_err(|e| format!("GET {at}: redirect to {location}: {e}"))?;
        if !same_origin(&next, &origin) {
            // Off the hub (the CDN): no token, and reqwest follows the rest.
            let resp = follow
                .get(next.clone())
                .send()
                .await
                .map_err(|e| format!("GET {next}: {e}"))?;
            return Ok((resp, commit));
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
            .build()
            .unwrap();
        (direct, reqwest::Client::new())
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
        let (resp, commit) = get_with_commit(&direct, &follow, "tok", &url)
            .await
            .unwrap();
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
        let (resp, commit) = get_with_commit(&direct, &follow, "tok", &url("config.json"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(commit.as_deref(), Some(COMMIT));
        let (_, commit) = get_with_commit(&direct, &follow, "", &url("plain.gguf"))
            .await
            .unwrap();
        assert_eq!(commit, None, "no header, no commit — never a guess");
        let (resp, commit) = get_with_commit(&direct, &follow, "", &url("gone.gguf"))
            .await
            .unwrap();
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
        let (resp, commit) = get_with_commit(&direct, &follow, "tok", &url)
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(commit.as_deref(), Some(COMMIT), "from the second hub hop");
        assert_eq!(resp.bytes().await.unwrap().as_ref(), b"GGUF");

        let looped = format!("{}/a/r/resolve/main/x", hub.uri());
        let err = get_with_commit(&direct, &follow, "", &looped)
            .await
            .unwrap_err();
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
            get_with_commit(&direct, &follow, "", &url),
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
