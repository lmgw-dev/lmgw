//! A repo's file tree from the hub API, every page of it.
//!
//! The hub pages a tree listing (`Link: <…>; rel="next"`, a cursor per page).
//! Reading only the first page made every file past it "not found" in a big
//! repo, so the pages are followed to the end — there is no page cap, and a
//! page that fails fails the whole listing, because a partial list would
//! report files missing that are not.
//!
//! The token goes along to every page, so a next link is only followed where
//! it stays on the hub: it is resolved as a URL against the page it came from
//! (RFC 3986 — a root-relative or protocol-relative link means what it means
//! there, not what string concatenation makes of it), and then has to share
//! the hub base's scheme, host and port and sit under its path.

use super::{hub_refusal, next_link, parse_tree_json, ratelimit_reset, validate_repo, HfFile};

/// Why a tree listing failed. Most callers only want the sentence
/// (`String::from`); the catalog refresh also needs to know a rate limit
/// apart, to stop asking once the window is used up.
#[derive(Debug, Clone, PartialEq)]
pub enum ListFailure {
    /// The hub's rate limit is used up; `reset_s` is when the window resets,
    /// when the hub said.
    RateLimited {
        message: String,
        reset_s: Option<u64>,
    },
    /// The repo has no such revision (the hub's `RevisionNotFound`).
    NoRevision(String),
    Other(String),
}

impl ListFailure {
    pub fn message(&self) -> &str {
        match self {
            ListFailure::RateLimited { message, .. } => message,
            ListFailure::NoRevision(m) | ListFailure::Other(m) => m,
        }
    }
}

impl From<ListFailure> for String {
    fn from(f: ListFailure) -> String {
        match f {
            ListFailure::RateLimited { message, .. } => message,
            ListFailure::NoRevision(m) | ListFailure::Other(m) => m,
        }
    }
}

impl From<String> for ListFailure {
    fn from(m: String) -> Self {
        ListFailure::Other(m)
    }
}

/// Files of a repo at its `main` branch, every page of the listing.
pub async fn list_repo_files(
    http: &reqwest::Client,
    token: &str,
    repo: &str,
) -> Result<Vec<super::HfFile>, String> {
    list_tree(http, token, &super::hf_base(), repo, "main")
        .await
        .map_err(String::from)
}

/// Files of a repo at `revision` (a branch or a commit), every page of the
/// listing (`GET {base}/api/models/{repo}/tree/{revision}?recursive=true`).
pub async fn list_tree(
    http: &reqwest::Client,
    token: &str,
    base: &str,
    repo: &str,
    revision: &str,
) -> Result<Vec<HfFile>, ListFailure> {
    validate_repo(repo)?;
    super::validate_revision(revision)?;
    let mut url = format!("{base}/api/models/{repo}/tree/{revision}?recursive=true");
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    loop {
        let (files, next) = list_page(http, token, repo, revision, &url).await?;
        out.extend(files);
        let Some(next) = next else {
            return Ok(out);
        };
        let next = next_page(base, &url, &next)?;
        seen.insert(url);
        if seen.contains(&next) {
            return Err(ListFailure::Other(format!(
                "HF API: the tree listing of {repo} pages in a loop ({next})"
            )));
        }
        url = next;
    }
}

/// The next page's URL: `next` resolved against `current` (the page whose
/// `Link` named it), refused unless it stays on the hub at `base`.
pub(super) fn next_page(base: &str, current: &str, next: &str) -> Result<String, String> {
    let refuse = || format!("HF API {current}: the next page is not on the hub ({next})");
    let hub = reqwest::Url::parse(base).map_err(|e| format!("HF base {base}: {e}"))?;
    let url = reqwest::Url::parse(current)
        .and_then(|c| c.join(next))
        .map_err(|_| refuse())?;
    let prefix = hub.path().trim_end_matches('/');
    let under = prefix.is_empty()
        || url.path() == prefix
        || url
            .path()
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'));
    let same_origin = url.scheme() == hub.scheme()
        && url.host_str() == hub.host_str()
        && url.port_or_known_default() == hub.port_or_known_default();
    // Credentials in the link would ride along as basic auth.
    let no_userinfo = url.username().is_empty() && url.password().is_none();
    match same_origin && under && no_userinfo {
        true => Ok(url.to_string()),
        false => Err(refuse()),
    }
}

/// One page of a tree listing, and the next page's URL when there is one.
async fn list_page(
    http: &reqwest::Client,
    token: &str,
    repo: &str,
    revision: &str,
    url: &str,
) -> Result<(Vec<HfFile>, Option<String>), ListFailure> {
    let mut rb = http.get(url);
    if !token.is_empty() {
        rb = rb.bearer_auth(token);
    }
    // A connected but silent hub would otherwise hold the caller (and a
    // catalog refresh with it) forever; this bounds the body too.
    let resp = rb
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("HF API: {e}"))?;
    let status = resp.status();
    let next = resp
        .headers()
        .get(reqwest::header::LINK)
        .and_then(|v| v.to_str().ok())
        .and_then(next_link);
    let reset = ratelimit_reset(resp.headers());
    let no_revision = resp
        .headers()
        .get("x-error-code")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| c == "RevisionNotFound");
    let body = resp.text().await.map_err(|e| format!("HF API: {e}"))?;
    if !status.is_success() {
        // A gated repo answers 401 to the *listing* too, so "which files does
        // this have" fails before a download is ever queued — which is why the
        // sentence belongs here and not only on the transfer (§2.7).
        if let Some(sentence) = hub_refusal(status, &body, repo, !token.is_empty()) {
            return Err(ListFailure::Other(sentence));
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(ListFailure::RateLimited {
                message: format!(
                    "Hugging Face rate limit reached listing {repo}{}",
                    reset
                        .map(|t| format!(" — it resets in {t} s"))
                        .unwrap_or_default()
                ),
                reset_s: reset,
            });
        }
        if no_revision {
            return Err(ListFailure::NoRevision(format!(
                "{repo} has no revision {revision} on Hugging Face"
            )));
        }
        return Err(ListFailure::Other(format!("HF API {url}: {status}")));
    }
    Ok((parse_tree_json(&body)?, next))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const HUB: &str = "https://huggingface.co";
    const PAGE: &str = "https://huggingface.co/api/models/a/b/tree/main?recursive=true";

    #[test]
    fn next_links_resolve_as_urls_against_the_page() {
        let ok = |next: &str| next_page(HUB, PAGE, next);
        // Absolute on the hub, root-relative, relative to the page.
        assert_eq!(
            ok("https://huggingface.co/api/models/a/b/tree/main?cursor=x").unwrap(),
            "https://huggingface.co/api/models/a/b/tree/main?cursor=x"
        );
        assert_eq!(
            ok("/api/models/a/b/tree/main?cursor=x").unwrap(),
            "https://huggingface.co/api/models/a/b/tree/main?cursor=x"
        );
        assert_eq!(
            ok("main?cursor=x").unwrap(),
            "https://huggingface.co/api/models/a/b/tree/main?cursor=x"
        );
        // Protocol-relative: the host it names, not `{base}//host/…`.
        assert_eq!(
            ok("//huggingface.co/api/x").unwrap(),
            "https://huggingface.co/api/x"
        );
        for foreign in [
            "//other.host/api/x",
            "https://other.host/api/x",
            "https://huggingface.co.evil.example/api/x",
            "https://huggingface.co@evil.example/api/x",
            "https://user:pw@huggingface.co/api/x",
            "http://huggingface.co/api/x",
            "https://huggingface.co:8443/api/x",
        ] {
            let err = ok(foreign).unwrap_err();
            assert!(err.contains("not on the hub"), "{foreign}: {err}");
        }
    }

    /// A mirror with a path: a root-relative link means the host's root, which
    /// is not the mirror — refused, not silently put under the mirror's path.
    #[test]
    fn a_base_with_a_path_keeps_the_token_under_it() {
        let base = "https://mirror.example/hf";
        let page = "https://mirror.example/hf/api/models/a/b/tree/main?recursive=true";
        assert_eq!(
            next_page(base, page, "?recursive=true&cursor=2").unwrap(),
            "https://mirror.example/hf/api/models/a/b/tree/main?recursive=true&cursor=2"
        );
        assert!(next_page(base, page, "/api/models/a/b/tree/main").is_err());
        assert!(next_page(base, page, "/hfx/api").is_err());
        assert!(next_page(base, page, "https://huggingface.co/api/models/a/b").is_err());
        // The port is part of the origin, and a default port is the same one.
        assert!(next_page(
            "http://127.0.0.1:1234",
            "http://127.0.0.1:1234/p",
            "http://127.0.0.1:12345/p"
        )
        .is_err());
        assert!(next_page(HUB, PAGE, "https://huggingface.co:443/api/x").is_ok());
    }

    /// Two pages with a token: the token reaches both (the mocks only answer
    /// with it), the relative next link is followed, and a next link that
    /// leaves the hub is refused before the token is sent there.
    #[tokio::test]
    async fn the_token_rides_every_page_and_never_leaves_the_hub() {
        let hub = MockServer::start().await;
        let other = MockServer::start().await;
        let tree = "/api/models/a/b/tree/main";
        Mock::given(method("GET"))
            .and(path(tree))
            .and(header("authorization", "Bearer tok"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header(
                        "link",
                        format!("<{tree}?recursive=true&cursor=p2>; rel=\"next\"").as_str(),
                    )
                    .set_body_json(serde_json::json!([{"type": "file", "path": "one.gguf"}])),
            )
            .mount(&hub)
            .await;
        Mock::given(method("GET"))
            .and(path(tree))
            .and(query_param("cursor", "p2"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!([{"type": "file", "path": "two.gguf"}])),
            )
            .with_priority(1)
            .mount(&hub)
            .await;
        let http = reqwest::Client::new();
        let files = list_tree(&http, "tok", &hub.uri(), "a/b", "main")
            .await
            .unwrap();
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["one.gguf", "two.gguf"]);

        // The same listing whose second page points at another host.
        let hub = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(tree))
            .and(header("authorization", "Bearer tok"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header(
                        "link",
                        format!("<{}{tree}?cursor=p2>; rel=\"next\"", other.uri()).as_str(),
                    )
                    .set_body_json(serde_json::json!([{"type": "file", "path": "one.gguf"}])),
            )
            .mount(&hub)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .expect(0)
            .mount(&other)
            .await;
        let err = list_tree(&http, "tok", &hub.uri(), "a/b", "main")
            .await
            .unwrap_err();
        assert!(err.message().contains("not on the hub"), "{err:?}");
    }

    #[tokio::test]
    async fn a_rate_limit_and_a_missing_revision_are_told_apart() {
        let hub = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/models/a/b/tree/main"))
            .respond_with(ResponseTemplate::new(429).insert_header("ratelimit", "\"api\";r=0;t=55"))
            .mount(&hub)
            .await;
        Mock::given(method("GET"))
            .and(path(
                "/api/models/a/b/tree/0123456789abcdef0123456789abcdef01234567",
            ))
            .respond_with(
                ResponseTemplate::new(404).insert_header("x-error-code", "RevisionNotFound"),
            )
            .mount(&hub)
            .await;
        let http = reqwest::Client::new();
        let err = list_tree(&http, "", &hub.uri(), "a/b", "main")
            .await
            .unwrap_err();
        assert_eq!(
            err,
            ListFailure::RateLimited {
                message: "Hugging Face rate limit reached listing a/b — it resets in 55 s".into(),
                reset_s: Some(55),
            }
        );
        let rev = "0123456789abcdef0123456789abcdef01234567";
        let err = list_tree(&http, "", &hub.uri(), "a/b", rev)
            .await
            .unwrap_err();
        assert!(matches!(err, ListFailure::NoRevision(_)), "{err:?}");
        assert!(err.message().contains(rev), "{err:?}");
    }
}
